//! Unit tests of the scheduler's forms, values and scripts (no server).

use super::*;

fn map(v: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

fn fv(v: Value) -> FormValues {
    FormValues {
        values: map(v),
        lists: BTreeMap::new(),
    }
}

fn j(s: &str) -> J {
    J::parse(s).unwrap()
}

fn es_ctx() -> Ctx {
    let all = vec![Type::Transform, Type::Watch, Type::Slm];
    Ctx {
        os: false,
        plugins: "_plugins",
        opendistro: false,
        types: all.clone(),
        manage: all,
        slm_running: true,
        lifecycle: true,
        manage_lifecycle: true,
        reset: true,
        notes: Vec::new(),
    }
}

fn os_ctx(opendistro: bool) -> Ctx {
    let all = if opendistro {
        vec![Type::Monitor]
    } else {
        vec![Type::OsTransform, Type::Monitor, Type::Sm]
    };
    Ctx {
        os: true,
        plugins: if opendistro {
            "_opendistro"
        } else {
            "_plugins"
        },
        opendistro,
        types: all.clone(),
        manage: all,
        slm_running: true,
        lifecycle: true,
        manage_lifecycle: true,
        reset: false,
        notes: Vec::new(),
    }
}

fn choices() -> Choices {
    let mut c = Choices::default();
    let mut put = |k: &str, values: &[&str]| {
        c.lists.insert(
            k.into(),
            values
                .iter()
                .map(|v| (v.to_string(), v.to_string()))
                .collect(),
        );
    };
    put("repository", &["repo1", "repo2"]);
    put("pipeline", &["", "p1"]);
    put("feature", &["kibana", "security"]);
    put("channel", &["", "ch1"]);
    c
}

/// A form's fields' defaults under `values`.
fn with_defaults(sections: &[FormSection], values: Value) -> FormValues {
    let mut v = defaults(sections);
    for (k, x) in map(values) {
        v.insert(k, x);
    }
    FormValues {
        values: v,
        lists: BTreeMap::new(),
    }
}

fn err(r: Result<Plan>) -> String {
    match r {
        Err(e) => e.to_string(),
        Ok(p) => panic!("expected an error, got:\n{}", p.render()),
    }
}

// ---------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------

#[test]
fn kinds_types_and_ids() {
    assert!(check_kind("job").is_ok());
    assert!(matches!(check_kind("schedule"), Err(Error::Unsupported(_))));
    assert_eq!(Type::OsTransform.job_id("a b"), "transform:a b");
    assert_eq!(Type::Slm.job_id("p"), "slm:p");
    assert_eq!(Type::Sm.job_id("p"), "sm:p");
    assert_eq!(Type::of_job(JobKind::Transform, true), Type::OsTransform);
    assert_eq!(Type::of_job(JobKind::Transform, false), Type::Transform);
    for ok in ["w", "Watch 1", "a.b", "x/y"] {
        assert_eq!(plain_id(Some(ok.into()), "Id").unwrap(), ok);
    }
    for bad in [
        "",
        "_all",
        "_x",
        "*",
        "a,b",
        "..",
        " x",
        "a\nb",
        &"a".repeat(256),
    ] {
        assert!(plain_id(Some(bad.into()), "Id").is_err(), "{bad:?}");
    }
    assert!(plain_id(None, "Id").is_err());
    // Whatever an id holds stays one path segment.
    let p = slm::delete_plan(&slm::Cur {
        id: "../_cluster/settings?x".into(),
        entry: J::Null,
    });
    assert!(p
        .render()
        .contains("DELETE /_slm/policy/..%2F_cluster%2Fsettings%3Fx\n"));
}

#[test]
fn values_are_checked() {
    let v = map(json!({
        "t": "30s", "bad_t": "5 min", "n": "12", "big": 99, "frac": 1.5,
        "obj": "{\"a\": 1}", "arr": "[1]", "broken": "{", "list": "a, b,,\nc",
        "cron6": "0  30 1 * * ?", "cron5": "0 1 * * *", "ctl": "a\u{7}b"
    }));
    let v = Vals(&v);
    assert_eq!(v.time("t", "T").unwrap().as_deref(), Some("30s"));
    assert!(v.time("bad_t", "T").is_err());
    assert_eq!(v.time("missing", "T").unwrap(), None);
    assert_eq!(v.int("n", "N", 1, 20).unwrap(), Some(12));
    assert!(v.int("big", "N", 1, 20).is_err());
    assert!(v.int("frac", "N", 1, 20).is_err());
    assert!(v.json("obj", "O", Shape::Object).unwrap().is_some());
    assert!(v.json("obj", "O", Shape::Array).is_err());
    assert!(v.json("arr", "A", Shape::Array).unwrap().is_some());
    assert!(v.json("broken", "B", Shape::Object).is_err());
    assert_eq!(v.strings("list"), ["a", "b", "c"]);
    assert_eq!(
        v.cron("cron6", "C", 6..=7).unwrap().as_deref(),
        Some("0 30 1 * * ?")
    );
    assert!(v.cron("cron5", "C", 6..=7).is_err());
    assert!(v.cron("cron6", "C", 5..=5).is_err());
    assert!(v.line("ctl", "L").is_err());
    for t in ["1s", "500ms", "7d", "100micros", "10nanos"] {
        assert!(is_time(t), "{t}");
    }
    for t in ["", "s", "1w", "1.5h", "-1s", "1 s"] {
        assert!(!is_time(t), "{t}");
    }
    assert_eq!(ms_time(600_000), "10m");
    assert_eq!(ms_time(7_200_000), "2h");
    assert_eq!(ms_time(1500), "1500ms");
}

#[test]
fn json_is_compared_by_meaning() {
    assert!(same(
        &j(r#"{"a":1,"b":[1,2]}"#),
        &j(r#"{"b":[1,2],"a":1.0}"#)
    ));
    assert!(!same(&j(r#"{"a":1}"#), &j(r#"{"a":1,"b":2}"#)));
    assert!(!same(&j("[1,2]"), &j("[2,1]")));
    assert!(same_opt(None, None) && !same_opt(Some(&J::Null), None));
}

#[test]
fn scripts_mask_secrets_but_keep_the_redaction_marker() {
    let b = j(
        r#"{"auth":{"basic":{"username":"u","password":"p4ss"}},"headers":{"Authorization":"Basic xyz"},"other":{"password":"::es_redacted::"},"list":[{"api_key":"k"}],"text":"password"}"#,
    );
    let m = mask(&b).compact();
    assert!(
        !m.contains("p4ss") && !m.contains("xyz") && !m.contains("\"k\""),
        "{m}"
    );
    assert!(m.contains("::es_redacted::") && m.contains(SECRET_MASK));
    assert!(m.contains(r#""text":"password""#));
    assert!(has_redacted(&b) && !has_redacted(&j(r#"{"a":"b"}"#)));
    // The plan renders masked; the request keeps the real value.
    let plan = Plan {
        notes: vec!["Watch «a»\nwith a line".into()],
        reqs: vec![Req::new("PUT", "/_watcher/watch/a".into(), Some(b.clone())).note("x")],
        ..Plan::default()
    };
    let r = plan.render();
    assert!(
        r.starts_with("# Watch «a» with a line\n\n# x\nPUT /_watcher/watch/a\n"),
        "{r}"
    );
    assert!(!r.contains("p4ss"));
    assert_eq!(plan.reqs[0].body.as_ref().unwrap(), &b);
    let empty = Plan {
        name: "x\ny".into(),
        ..Plan::default()
    };
    assert_eq!(empty.render(), "# «x y»: no hay cambios que guardar.\n");
}

/// Every field of a new job's form shows only for its type (its `when` or
/// one of its `when_all` names the type), and so does every tab but
/// "General", which holds the type.
fn gated(sections: &[FormSection]) {
    assert_eq!(sections[0].key, "general");
    assert!(sections[0].when.is_none());
    for s in &sections[1..] {
        let Some((k, v)) = &s.when else {
            panic!("tab {} shows for every type", s.key);
        };
        assert_eq!(k, "type", "{}", s.key);
        assert!(s.key.starts_with(&format!("{}_", v[0])), "{}", s.key);
    }
    for (i, s) in sections.iter().enumerate() {
        for f in &s.fields {
            if f.key == "type" {
                assert!(f.when.is_none() && f.when_all.is_empty());
                continue;
            }
            let t = type_of(f.when.as_ref(), &f.when_all)
                .unwrap_or_else(|| panic!("{} shows for every type", f.key));
            if i > 0 {
                assert_eq!(Some(&t), s.when.as_ref().map(|w| &w.1[0]), "{}", f.key);
            }
        }
    }
}

#[test]
fn a_new_jobs_form_shows_each_types_tabs_only_for_it() {
    let ch = choices();
    for c in [es_ctx(), os_ctx(false), os_ctx(true)] {
        let parts = c
            .types
            .iter()
            .map(|t| (*t, sections_of(&c, *t, true, &ch)))
            .collect();
        let sections = combined(&c, parts);
        assert_eq!(sections[0].key, "general");
        assert_eq!(sections[0].fields[0].key, "type");
        assert!(sections.iter().all(|s| s.lists.is_empty()));
        gated(&sections);
        let FormKind::Select { options } = &sections[0].fields[0].kind else {
            panic!()
        };
        assert_eq!(options.len(), c.types.len());
        // Each type's own tabs, as an existing job of that type has them.
        for t in &c.types {
            let own: Vec<String> = sections_of(&c, *t, true, &ch)
                .iter()
                .filter(|s| s.key != "general")
                .map(|s| format!("{}_{}", t.key(), s.key))
                .collect();
            let shown: Vec<String> = sections
                .iter()
                .filter(|s| s.when.as_ref().is_some_and(|w| w.1 == [t.key()]))
                .map(|s| s.key.clone())
                .collect();
            assert_eq!(shown, own, "{t:?}");
        }
    }
    // A watch's trigger fields follow the schedule's kind and the type.
    let c = es_ctx();
    let sections = combined(&c, vec![(Type::Watch, watch::sections())]);
    let interval = sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|f| f.key == "interval")
        .unwrap();
    assert_eq!(
        interval.when,
        Some(("trigger_kind".into(), vec!["interval".into()]))
    );
    assert_eq!(
        interval.when_all,
        [("type".to_string(), vec!["watch".to_string()])]
    );
    // A type the user may not manage isn't offered.
    let mut c = es_ctx();
    c.manage = vec![Type::Transform];
    let f = type_field(&c, None);
    let FormKind::Select { options } = &f.kind else {
        panic!()
    };
    assert_eq!(
        options,
        &[("transform".to_string(), "Transform".to_string())]
    );
    // An existing job's type is shown, read only.
    let f = type_field(&es_ctx(), Some(Type::Slm));
    assert!(f.read_only);
}

#[test]
fn the_type_is_checked() {
    let c = es_ctx();
    assert_eq!(
        wanted_type(&c, &fv(json!({"type": "watch"}))).unwrap(),
        Type::Watch
    );
    assert!(wanted_type(&c, &fv(json!({"type": "monitor"}))).is_err());
    assert!(wanted_type(&c, &fv(json!({}))).is_err());
}

// ---------------------------------------------------------------------------
// Elasticsearch transforms
// ---------------------------------------------------------------------------

fn tf_new(extra: Value) -> FormValues {
    let mut v = with_defaults(
        &es_transform::sections(true),
        json!({
            "id": "ventas",
            "description": "por cliente",
            "source_index": "ventas-*, clientes",
            "source_query": "{\"term\": {\"pais\": \"AR\"}}",
            "group_by": "{\"cliente\": {\"terms\": {\"field\": \"cliente\"}}}",
            "aggregations": "{\"total\": {\"sum\": {\"field\": \"importe\"}}}",
            "dest_index": "ventas-por-cliente",
            "continuous": true,
            "sync_field": "@timestamp",
        }),
    );
    for (k, x) in map(extra) {
        v.values.insert(k, x);
    }
    v
}

fn tf_cur(def: &str, state: &str) -> es_transform::Cur {
    es_transform::Cur {
        id: "ventas".into(),
        def: j(def),
        state: state.into(),
        managed: false,
    }
}

const TF_DEF: &str = r#"{"id":"ventas","version":"10.0.0","create_time":1,"source":{"index":["ventas-*","clientes"],"query":{"term":{"pais":"AR"}}},"dest":{"index":"ventas-por-cliente","pipeline":"p1"},"frequency":"1m","sync":{"time":{"field":"@timestamp","delay":"60s"}},"pivot":{"group_by":{"cliente":{"terms":{"field":"cliente"}}},"aggregations":{"total":{"sum":{"field":"importe"}}}},"description":"por cliente","settings":{"max_page_search_size":500},"_meta":{"a":1}}"#;

#[test]
fn es_transform_create() {
    let p = es_transform::plan(None, &tf_new(json!({"defer_validation": true}))).unwrap();
    let r = p.render();
    assert!(
        r.starts_with("# Transform «ventas»\n\nPUT /_transform/ventas?defer_validation=true\n"),
        "{r}"
    );
    assert!(r.contains("\nPOST /_transform/ventas/_start\n"), "{r}");
    let body = p.reqs[0].body.as_ref().unwrap();
    assert_eq!(
        body.at(&["source", "index"]).unwrap().compact(),
        r#"["ventas-*","clientes"]"#
    );
    assert_eq!(
        body.at(&["sync", "time"]).unwrap().compact(),
        r#"{"field":"@timestamp"}"#
    );
    // Settings at their defaults aren't written.
    assert!(body.get("settings").is_none(), "{}", body.compact());
    assert_eq!(p.id.as_deref(), Some("transform:ventas"));
    // Stopped, a latest, with settings.
    let p = es_transform::plan(
        None,
        &tf_new(json!({
            "enabled": false, "function": "latest", "unique_key": "cliente, producto",
            "sort": "@timestamp", "continuous": false, "unattended": true,
            "num_failure_retries": 3, "docs_per_second": "12.5"
        })),
    )
    .unwrap();
    assert_eq!(p.reqs.len(), 1);
    let body = p.reqs[0].body.as_ref().unwrap();
    assert_eq!(
        body.get("latest").unwrap().compact(),
        r#"{"unique_key":["cliente","producto"],"sort":"@timestamp"}"#
    );
    assert!(body.get("pivot").is_none() && body.get("sync").is_none());
    assert_eq!(
        body.get("settings").unwrap().compact(),
        r#"{"docs_per_second":12.5,"num_failure_retries":3,"unattended":true}"#
    );
}

#[test]
fn es_transform_is_checked() {
    for (k, x, says) in [
        ("id", json!("Ventas"), "id de transform"),
        ("id", json!("a_"), "id de transform"),
        ("source_index", json!(" , "), "índice de origen"),
        ("group_by", json!(""), "group_by"),
        ("aggregations", json!("[1]"), "objeto JSON"),
        ("dest_index", json!("a\nb"), "saltos de línea"),
        ("frequency", json!("5 m"), "tiempo válido"),
        ("sync_field", json!(""), "campo de tiempo"),
        ("max_page_search_size", json!(5), "entre 10 y 65536"),
        ("function", json!("sum"), "función"),
        ("retention_field", json!("t"), "retención"),
    ] {
        let e = err(es_transform::plan(None, &tf_new(json!({ k: x }))));
        assert!(e.contains(says), "{k}: {e}");
    }
}

#[test]
fn es_transform_changes_in_place() {
    let cur = tf_cur(TF_DEF, "started");
    let same_values = FormValues {
        values: es_transform::values_of(&cur),
        lists: BTreeMap::new(),
    };
    let p = es_transform::plan(Some(&cur), &same_values).unwrap();
    assert!(p.reqs.is_empty(), "{}", p.render());
    assert!(p.render().contains("no hay cambios"));
    let mut v = same_values.clone();
    for (k, x) in map(json!({
        "description": "", "frequency": "5m", "dest_pipeline": "", "sync_delay": "2m",
        "retention_field": "t", "retention_max_age": "30d", "max_page_search_size": 500,
        "docs_per_second": 50, "meta": "", "enabled": false
    })) {
        v.values.insert(k, x);
    }
    let p = es_transform::plan(Some(&cur), &v).unwrap();
    assert_eq!(p.reqs.len(), 2, "{}", p.render());
    assert_eq!(p.reqs[0].path, "/_transform/ventas/_update");
    assert_eq!(
        p.reqs[0].body.as_ref().unwrap().compact(),
        r#"{"description":"","dest":{"index":"ventas-por-cliente"},"frequency":"5m","sync":{"time":{"field":"@timestamp","delay":"2m"}},"retention_policy":{"time":{"field":"t","max_age":"30d"}},"settings":{"docs_per_second":50},"_meta":{}}"#
    );
    assert_eq!(p.reqs[1].path, "/_transform/ventas/_stop");
    // Removing the retention and a setting sends null.
    let cur2 = tf_cur(
        &TF_DEF.replace(
            r#""_meta":{"a":1}"#,
            r#""_meta":{"a":1},"retention_policy":{"time":{"field":"t","max_age":"30d"}},"settings":{"docs_per_second":5}"#,
        ),
        "stopped",
    );
    let mut v = FormValues {
        values: es_transform::values_of(&cur2),
        lists: BTreeMap::new(),
    };
    for k in ["retention_field", "retention_max_age", "docs_per_second"] {
        v.values.insert(k.into(), json!(""));
    }
    let p = es_transform::plan(Some(&cur2), &v).unwrap();
    assert_eq!(
        p.reqs[0].body.as_ref().unwrap().compact(),
        r#"{"retention_policy":null,"settings":{"docs_per_second":null}}"#
    );
    // A failed transform stops only when forced.
    let cur3 = tf_cur(TF_DEF, "failed");
    let mut v = FormValues {
        values: es_transform::values_of(&cur3),
        lists: BTreeMap::new(),
    };
    v.values.insert("enabled".into(), json!(false));
    let p = es_transform::plan(Some(&cur3), &v).unwrap();
    assert_eq!(p.reqs[0].path, "/_transform/ventas/_stop?force=true");
}

#[test]
fn es_transform_recreated_when_the_api_cant_change_it() {
    let cur = tf_cur(TF_DEF, "started");
    let base = es_transform::values_of(&cur);
    for change in [
        json!({"aggregations": "{\"total\": {\"max\": {\"field\": \"importe\"}}}"}),
        json!({"continuous": false}),
        json!({"function": "latest", "unique_key": "cliente", "sort": "t"}),
    ] {
        let mut v = FormValues {
            values: base.clone(),
            lists: BTreeMap::new(),
        };
        for (k, x) in map(change.clone()) {
            v.values.insert(k, x);
        }
        let p = es_transform::plan(Some(&cur), &v).unwrap();
        let r = p.render();
        assert!(r.contains(es_transform::RECREATE_NOTE), "{change}: {r}");
        let paths: Vec<(&str, &str)> = p.reqs.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert_eq!(
            paths,
            [
                ("POST", "/_transform/_preview"),
                ("DELETE", "/_transform/ventas?force=true"),
                ("PUT", "/_transform/ventas"),
                ("POST", "/_transform/ventas/_start"),
            ],
            "{change}"
        );
        // The preview gets no description, _meta or retention.
        let preview = p.reqs[0].body.as_ref().unwrap();
        assert!(preview.get("description").is_none() && preview.get("_meta").is_none());
    }
    // A new id: the new one first, then the old one goes.
    let mut v = FormValues {
        values: base,
        lists: BTreeMap::new(),
    };
    v.values.insert("id".into(), json!("ventas-2"));
    let p = es_transform::plan(Some(&cur), &v).unwrap();
    let paths: Vec<&str> = p.reqs.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/_transform/ventas-2",
            "/_transform/ventas?force=true",
            "/_transform/ventas-2/_start"
        ]
    );
    assert_eq!(p.id.as_deref(), Some("transform:ventas-2"));
    let d = es_transform::delete_plan(&cur).render();
    assert!(d.contains("DELETE /_transform/ventas?force=true\n") && d.contains("se conserva"));
}

#[test]
fn es_transform_settings_kept_at_their_default_stay() {
    // The cluster has max_page_search_size: 500 set explicitly: kept.
    let cur = tf_cur(TF_DEF, "stopped");
    let v = es_transform::values_of(&cur);
    let t = es_transform::spec(&v, &["max_page_search_size".to_string()]).unwrap();
    assert_eq!(
        t.settings,
        vec![("max_page_search_size".to_string(), J::Num(500.into()))]
    );
    let t = es_transform::spec(&v, &[]).unwrap();
    assert!(t.settings.is_empty());
    assert!(!t.started);
}

// ---------------------------------------------------------------------------
// Watches
// ---------------------------------------------------------------------------

fn watch_cur(watch: &str, active: bool) -> watch::Cur {
    watch::Cur {
        id: "errores".into(),
        watch: j(watch),
        active,
        ackable: false,
        seq: "7".into(),
        term: "2".into(),
    }
}

const WATCH: &str = r#"{"trigger":{"schedule":{"weekly":[{"on":["MON","FRI"],"at":["09:00","17:30"]}]}},"input":{"none":{}},"condition":{"always":{}},"throttle_period_in_millis":600000,"actions":{"uno":{"logging":{"level":"info","text":"uno"}},"hook":{"webhook":{"host":"h","port":80,"auth":{"basic":{"username":"u","password":"::es_redacted::"}}}},"dos":{"logging":{"text":"dos"}}},"metadata":{"name":"Errores","umbral":3}}"#;

#[test]
fn watch_triggers_round_trip() {
    for schedule in [
        r#"{"interval":"10m"}"#,
        r#"{"cron":"0 0/5 * * * ?"}"#,
        r#"{"cron":["0 0 1 * * ?","0 0 13 * * ?"]}"#,
        r#"{"hourly":{"minute":[0,30]}}"#,
        r#"{"daily":{"at":["09:00","18:30"]}}"#,
        r#"{"weekly":{"on":["monday","friday"],"at":["09:00"]}}"#,
        r#"{"monthly":{"on":[1,15],"at":["06:00"]}}"#,
        r#"{"yearly":{"in":["january","july"],"on":[1],"at":["00:00"]}}"#,
    ] {
        let cur = watch_cur(
            &format!(r#"{{"trigger":{{"schedule":{schedule}}},"actions":{{}}}}"#),
            true,
        );
        let v = watch::values_of(&cur);
        assert_ne!(v["trigger_kind"], json!("json"), "{schedule}");
        let back = watch::schedule(Vals(&v)).unwrap();
        // The cluster writes one entry as a list; the form, as an object.
        let want = j(schedule);
        let norm = |s: &J| {
            let (k, b) = s.as_obj().unwrap()[0].clone();
            let b = match b {
                J::Arr(a) if a.len() == 1 => a[0].clone(),
                b => b,
            };
            (k, b)
        };
        assert!(
            norm(&back).0 == norm(&want).0 && same(&norm(&back).1, &norm(&want).1),
            "{schedule} → {}",
            back.compact()
        );
    }
    // The cluster's abbreviations become the form's values.
    let v = watch::values_of(&watch_cur(WATCH, true));
    assert_eq!(v["trigger_kind"], json!("weekly"));
    assert_eq!(v["week_days"], json!(["monday", "friday"]));
    assert_eq!(v["times"], json!("09:00, 17:30"));
    assert_eq!(v["throttle_period"], json!("10m"));
    assert_eq!(v["description"], json!("Errores"));
    assert_eq!(v["metadata"], json!("{\n  \"umbral\": 3\n}"));
    // What the fields can't say stays JSON.
    for schedule in [
        r#"{"weekly":[{"on":["monday"],"at":["09:00"]},{"on":["friday"],"at":["18:00"]}]}"#,
        r#"{"daily":{"at":"noon"}}"#,
        r#"{"daily":{"at":[{"hour":[9],"minute":[0]}]}}"#,
    ] {
        let v = watch::values_of(&watch_cur(
            &format!(r#"{{"trigger":{{"schedule":{schedule}}}}}"#),
            true,
        ));
        assert_eq!(v["trigger_kind"], json!("json"), "{schedule}");
        assert!(same(&watch::schedule(Vals(&v)).unwrap(), &j(schedule)));
    }
}

#[test]
fn watch_values_are_checked() {
    let ok = map(json!({"id": "w", "trigger_kind": "interval", "interval": "5m"}));
    assert!(watch::spec(&ok).is_ok());
    for (k, x, says) in [
        ("interval", json!("5 min"), "Intervalo"),
        ("interval", json!("5y"), "Intervalo"),
        ("id", json!("_w"), "Id"),
        ("input", json!("[]"), "objeto JSON"),
        ("actions", json!("{"), "JSON válido"),
        ("throttle_period", json!("x"), "tiempo válido"),
    ] {
        let mut v = ok.clone();
        v.insert(k.into(), x);
        let e = watch::spec(&v).unwrap_err().to_string();
        assert!(e.contains(says), "{k}: {e}");
    }
    for (fields, says) in [
        (
            json!({"trigger_kind": "hourly", "minutes": "0, 60"}),
            "entre 0 y 59",
        ),
        (
            json!({"trigger_kind": "daily", "times": "25:00"}),
            "hora válida",
        ),
        (
            json!({"trigger_kind": "daily", "times": "9:5"}),
            "hora válida",
        ),
        (
            json!({"trigger_kind": "weekly", "times": "09:00", "week_days": ["mon"]}),
            "valor válido",
        ),
        (
            json!({"trigger_kind": "monthly", "times": "09:00", "month_days": "32"}),
            "entre 1 y 31",
        ),
        (
            json!({"trigger_kind": "cron", "cron": "0 1 * * *"}),
            "expresión cron",
        ),
        (
            json!({"trigger_kind": "json", "trigger_json": "{\"a\": 1, \"b\": 2}"}),
            "un solo tipo",
        ),
        (json!({"trigger_kind": "monthly2"}), "tipo de programación"),
    ] {
        let mut v = ok.clone();
        for (k, x) in map(fields.clone()) {
            v.insert(k, x);
        }
        let e = watch::spec(&v).unwrap_err().to_string();
        assert!(e.contains(says), "{fields}: {e}");
    }
}

#[test]
fn watch_create_update_and_delete() {
    let mut v = with_defaults(
        &watch::sections(),
        json!({
            "id": "errores", "description": "Errores", "enabled": false,
            "actions": "{\"hook\": {\"webhook\": {\"host\": \"h\", \"auth\": {\"basic\": {\"username\": \"u\", \"password\": \"s3cret\"}}}}}"
        }),
    );
    let p = watch::plan(None, &v).unwrap();
    let r = p.render();
    assert!(
        r.contains("PUT /_watcher/watch/errores?active=false\n"),
        "{r}"
    );
    assert!(r.contains(SECRET_MASK) && !r.contains("s3cret"), "{r}");
    assert!(p.reqs[0]
        .body
        .as_ref()
        .unwrap()
        .compact()
        .contains("s3cret"));
    let body = p.reqs[0].body.as_ref().unwrap();
    assert_eq!(
        body.at(&["trigger", "schedule", "interval"])
            .unwrap()
            .text(),
        "10m"
    );
    assert_eq!(body.at(&["metadata", "name"]).unwrap().text(), "Errores");
    // A new watch with a redacted value is refused.
    v.values.insert(
        "actions".into(),
        json!(
            "{\"a\": {\"webhook\": {\"auth\": {\"basic\": {\"password\": \"::es_redacted::\"}}}}}"
        ),
    );
    assert!(err(watch::plan(None, &v)).contains("::es_redacted::"));

    let cur = watch_cur(WATCH, true);
    let base = watch::values_of(&cur);
    let edit = |change: Value| {
        let mut v = FormValues {
            values: base.clone(),
            lists: BTreeMap::new(),
        };
        for (k, x) in map(change) {
            v.values.insert(k, x);
        }
        watch::plan(Some(&cur), &v)
    };
    assert!(edit(json!({})).unwrap().reqs.is_empty());
    // Values only, with the redacted password: an update with if_seq_no,
    // the throttle written as the stored watch writes it.
    let p = edit(json!({"times": "08:00", "throttle_period": "1m"})).unwrap();
    assert_eq!(
        p.reqs[0].path,
        "/_watcher/watch/errores?if_seq_no=7&if_primary_term=2&active=true"
    );
    let b = p.reqs[0].body.as_ref().unwrap();
    assert_eq!(b.get("throttle_period_in_millis").unwrap().text(), "60000");
    assert!(b.get("throttle_period").is_none());
    assert!(p.render().contains("conservar los valores"));
    // A change of shape with the password redacted: refused.
    for change in [
        json!({"trigger_kind": "interval", "interval": "5m"}),
        json!({"actions": "{\"uno\": {\"logging\": {\"text\": \"uno\"}}, \"hook\": {\"webhook\": {\"host\": \"h\", \"port\": 80, \"auth\": {\"basic\": {\"username\": \"u\", \"password\": \"::es_redacted::\"}}}}}"}),
        json!({"metadata": ""}),
    ] {
        let e = err(edit(change.clone()));
        assert!(e.contains("quita partes"), "{change}: {e}");
    }
    // With the password typed: replaced whole, without if_seq_no.
    let p = edit(json!({
        "trigger_kind": "interval", "interval": "5m",
        "actions": "{\"hook\": {\"webhook\": {\"host\": \"h\", \"auth\": {\"basic\": {\"username\": \"u\", \"password\": \"nuevo\"}}}}}"
    }))
    .unwrap();
    assert_eq!(p.reqs[0].path, "/_watcher/watch/errores?active=true");
    // Only "active".
    let p = edit(json!({"enabled": false})).unwrap();
    assert_eq!(p.reqs[0].path, "/_watcher/watch/errores/_deactivate");
    assert!(p.reqs[0].body.is_none());
    let p = watch::plan(
        Some(&watch_cur(WATCH, false)),
        &FormValues {
            values: {
                let mut b = base.clone();
                b.insert("enabled".into(), json!(true));
                b
            },
            lists: BTreeMap::new(),
        },
    )
    .unwrap();
    assert_eq!(p.reqs[0].path, "/_watcher/watch/errores/_activate");
    // A new id needs the real values.
    assert!(err(edit(json!({"id": "errores2"}))).contains("::es_redacted::"));
    let p = edit(json!({
        "id": "errores2",
        "actions": "{\"uno\": {\"logging\": {\"text\": \"uno\"}}}"
    }))
    .unwrap();
    let paths: Vec<&str> = p.reqs.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/_watcher/watch/errores2?active=true",
            "/_watcher/watch/errores"
        ]
    );
    assert_eq!(p.reqs[1].method, "DELETE");
    assert!(watch::delete_plan(&cur)
        .render()
        .contains("DELETE /_watcher/watch/errores\n"));
}

#[test]
fn watch_merge_is_the_clusters() {
    let base = j(r#"{"a":{"x":1,"y":2},"b":[1,2],"c":1}"#);
    let patch = j(r#"{"a":{"y":3},"b":[3]}"#);
    assert_eq!(
        watch::merge(&base, &patch).compact(),
        r#"{"a":{"x":1,"y":3},"b":[3],"c":1}"#
    );
}

#[test]
fn watch_starts_at_an_action() {
    let cur = watch_cur(WATCH, true);
    let b = watch::run_from_body(&cur, "hook").unwrap();
    assert_eq!(
        b.compact(),
        r#"{"record_execution":true,"action_modes":{"uno":"skip","hook":"force_execute","dos":"force_execute"}}"#
    );
    assert!(watch::run_from_body(&cur, "nope").is_err());
}

// ---------------------------------------------------------------------------
// SLM
// ---------------------------------------------------------------------------

const SLM: &str = r#"{"version":1,"policy":{"name":"<n-{now/d}>","schedule":"0 30 1 * * ?","repository":"repo1","config":{"indices":["a","b"],"include_global_state":false,"feature_states":["kibana"]},"retention":{"expire_after":"30d","min_count":1,"max_count":5}}}"#;

#[test]
fn slm_policies() {
    let ch = choices();
    let v = with_defaults(
        &slm::sections(),
        json!({
            "id": "noche", "snapshot_name": "<n-{now/d}>", "repository": "repo1",
            "slm_schedule": "0 30 1 * * ?", "indices": "a, b", "max_count": 5,
            "snapshot_metadata": "{\"por\": \"dbine\"}"
        }),
    );
    let p = slm::plan(None, &v, &ch).unwrap();
    assert_eq!(p.reqs[0].path, "/_slm/policy/noche");
    assert_eq!(
        p.reqs[0].body.as_ref().unwrap().compact(),
        r#"{"name":"<n-{now/d}>","schedule":"0 30 1 * * ?","repository":"repo1","config":{"indices":["a","b"],"include_global_state":true,"ignore_unavailable":false,"partial":false,"metadata":{"por":"dbine"}},"retention":{"max_count":5}}"#
    );
    for (k, x, says) in [
        ("repository", json!("nope"), "no existe"),
        ("slm_schedule", json!("0 1 * * *"), "expresión cron"),
        ("min_count", json!(9), "mayor que el máximo"),
        ("expire_after", json!("1 month"), "tiempo válido"),
        ("snapshot_name", json!(""), "nombre de los snapshots"),
    ] {
        let mut v = v.clone();
        v.values.insert(k.into(), x);
        let e = err(slm::plan(None, &v, &ch));
        assert!(e.contains(says), "{k}: {e}");
    }
    let mut v2 = v.clone();
    v2.values.insert("feature_states".into(), json!(["nope"]));
    assert!(err(slm::plan(None, &v2, &ch)).contains("no existe"));

    let cur = slm::Cur {
        id: "noche".into(),
        entry: j(SLM),
    };
    let base = FormValues {
        values: slm::values_of(&cur),
        lists: BTreeMap::new(),
    };
    assert_eq!(base.values["feature_states"], json!(["kibana"]));
    assert!(slm::plan(Some(&cur), &base, &ch).unwrap().reqs.is_empty());
    let mut v = base.clone();
    v.values.insert("max_count".into(), json!(9));
    let p = slm::plan(Some(&cur), &v, &ch).unwrap();
    assert_eq!(p.reqs.len(), 1);
    assert_eq!(
        (p.reqs[0].method, p.reqs[0].path.as_str()),
        ("PUT", "/_slm/policy/noche")
    );
    v.values.insert("id".into(), json!("noche2"));
    let p = slm::plan(Some(&cur), &v, &ch).unwrap();
    let paths: Vec<(&str, &str)> = p.reqs.iter().map(|r| (r.method, r.path.as_str())).collect();
    assert_eq!(
        paths,
        [
            ("PUT", "/_slm/policy/noche2"),
            ("DELETE", "/_slm/policy/noche")
        ]
    );
    assert_eq!(p.id.as_deref(), Some("slm:noche2"));
}

#[test]
fn elastic_clouds_snapshot_policy_keeps_its_id_and_repository() {
    let ch = choices();
    let cur = slm::Cur {
        id: "cloud-snapshot-policy".into(),
        entry: j(SLM),
    };
    let base = FormValues {
        values: slm::values_of(&cur),
        lists: BTreeMap::new(),
    };
    assert!(slm_plan(Some(&cur), &base, &ch).unwrap().reqs.is_empty());
    // A new id would create another policy and delete this one; another
    // repository would stop the deployment's snapshots: both refused.
    for (k, x) in [
        ("id", json!("otra")),
        ("repository", json!("repo2")),
        ("id", json!("")),
    ] {
        let mut v = base.clone();
        v.values.insert(k.into(), x);
        let e = err(slm_plan(Some(&cur), &v, &ch));
        assert!(
            e.contains("Elastic Cloud") && e.contains("repositorio"),
            "{k}: {e}"
        );
    }
    // Its schedule, retention and snapshots' options change in place.
    let mut v = base.clone();
    v.values.insert("slm_schedule".into(), json!("0 0 2 * * ?"));
    v.values.insert("max_count".into(), json!(9));
    v.values.insert("partial".into(), json!(true));
    let p = slm_plan(Some(&cur), &v, &ch).unwrap();
    let paths: Vec<(&str, &str)> = p.reqs.iter().map(|r| (r.method, r.path.as_str())).collect();
    assert_eq!(paths, [("PUT", "/_slm/policy/cloud-snapshot-policy")]);
    // Any other policy is renamed as before.
    let other = slm::Cur {
        id: "noche".into(),
        entry: j(SLM),
    };
    let mut v = FormValues {
        values: slm::values_of(&other),
        lists: BTreeMap::new(),
    };
    v.values.insert("id".into(), json!("noche2"));
    assert_eq!(slm_plan(Some(&other), &v, &ch).unwrap().reqs.len(), 2);
    // Its form shows the id and the repository read only.
    let mut sections = slm::sections();
    lock_cloud_slm(&mut sections);
    let locked: Vec<&str> = sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .filter(|f| f.read_only)
        .map(|f| f.key.as_str())
        .collect();
    assert_eq!(locked, ["id", "repository"]);
    assert!(sections[0].fields[0].help.is_none());
}

// ---------------------------------------------------------------------------
// OpenSearch transforms
// ---------------------------------------------------------------------------

const OS_TF: &str = r#"{"transform_id":"t","schema_version":21,"schedule":{"interval":{"start_time":1700000000000,"period":1,"unit":"Minutes"}},"updated_at":1,"enabled":true,"enabled_at":1,"description":"d","source_index":"src","data_selection_query":{"match_all":{"boost":1.0}},"target_index":"dst","page_size":5,"groups":[{"terms":{"source_field":"h","target_field":"h"}}],"aggregations":{"v":{"sum":{"field":"v"}}},"continuous":false}"#;

#[test]
fn os_transforms() {
    let c = os_ctx(false);
    let v = with_defaults(
        &os_transform::sections(),
        json!({
            "id": "t", "source_index": "src", "dest_index": "dst",
            "groups": "[{\"terms\": {\"source_field\": \"h\", \"target_field\": \"h\"}}]",
            "start_time": "2023-11-14T22:13:20Z"
        }),
    );
    let p = os_transform::plan(&c, None, &v).unwrap();
    assert_eq!(p.reqs[0].path, "/_plugins/_transform/t");
    assert_eq!(
        p.reqs[0].body.as_ref().unwrap().compact(),
        r#"{"transform":{"enabled":true,"continuous":false,"schedule":{"interval":{"start_time":1700000000000,"period":1,"unit":"Minutes"}},"description":"","source_index":"src","target_index":"dst","data_selection_query":{"match_all":{}},"page_size":1000,"groups":[{"terms":{"source_field":"h","target_field":"h"}}],"aggregations":{}}}"#
    );
    for (k, x, says) in [
        ("groups", json!("{}"), "lista JSON"),
        ("interval_unit", json!("weeks"), "Unidad"),
        ("page_size", json!(0), "entre 1 y 10000"),
        ("start_time", json!("mañana"), "fecha y hora"),
    ] {
        let mut v = v.clone();
        v.values.insert(k.into(), x);
        let e = err(os_transform::plan(&c, None, &v));
        assert!(e.contains(says), "{k}: {e}");
    }
    let mut cron = v.clone();
    for (k, x) in
        map(json!({"schedule_kind": "cron", "cron": "0 * * * *", "timezone": "Europe/Madrid"}))
    {
        cron.values.insert(k, x);
    }
    let p = os_transform::plan(&c, None, &cron).unwrap();
    assert_eq!(
        p.reqs[0]
            .body
            .as_ref()
            .unwrap()
            .at(&["transform", "schedule"])
            .unwrap()
            .compact(),
        r#"{"cron":{"expression":"0 * * * *","timezone":"Europe/Madrid"}}"#
    );
    for (k, x) in [("cron", "0 0 * * * ?"), ("timezone", "Europe Madrid")] {
        let mut v = cron.clone();
        v.values.insert(k.into(), json!(x));
        assert!(os_transform::plan(&c, None, &v).is_err(), "{k}");
    }

    let cur = os_transform::Cur {
        id: "t".into(),
        t: j(OS_TF),
        seq: "3".into(),
        term: "1".into(),
    };
    let base = os_transform::values_of(&cur);
    let edit = |change: Value| {
        let mut v = FormValues {
            values: base.clone(),
            lists: BTreeMap::new(),
        };
        for (k, x) in map(change) {
            v.values.insert(k, x);
        }
        os_transform::plan(&c, Some(&cur), &v).unwrap()
    };
    assert!(edit(json!({})).reqs.is_empty());
    let p = edit(json!({"description": "x", "page_size": 9, "enabled": false}));
    assert_eq!(p.reqs.len(), 1);
    assert_eq!(
        p.reqs[0].path,
        "/_plugins/_transform/t?if_seq_no=3&if_primary_term=1"
    );
    for change in [
        json!({"dest_index": "dst2"}),
        json!({"continuous": true}),
        json!({"source_query": "{\"term\": {\"h\": \"a\"}}"}),
    ] {
        let p = edit(change.clone());
        let paths: Vec<(&str, &str)> = p.reqs.iter().map(|r| (r.method, r.path.as_str())).collect();
        assert_eq!(
            paths,
            [
                ("POST", "/_plugins/_transform/_preview"),
                ("POST", "/_plugins/_transform/t/_stop"),
                ("DELETE", "/_plugins/_transform/t"),
                ("PUT", "/_plugins/_transform/t"),
            ],
            "{change}"
        );
        assert!(p.render().contains(os_transform::RECREATE_NOTE));
    }
    let p = edit(json!({"id": "t2"}));
    let paths: Vec<&str> = p.reqs.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "/_plugins/_transform/t2",
            "/_plugins/_transform/t/_stop",
            "/_plugins/_transform/t"
        ]
    );
    let d = os_transform::delete_plan("_plugins", &cur);
    assert_eq!(d.reqs.len(), 2);
    assert_eq!(d.reqs[1].method, "DELETE");
}

// ---------------------------------------------------------------------------
// Monitors
// ---------------------------------------------------------------------------

const MONITOR: &str = r#"{"type":"monitor","schema_version":0,"name":"m","monitor_type":"query_level_monitor","user":{"name":"admin"},"enabled":false,"enabled_time":null,"schedule":{"period":{"interval":5,"unit":"MINUTES"}},"inputs":[{"search":{"indices":["i"],"query":{"size":0}}}],"triggers":[{"query_level_trigger":{"id":"T1","name":"t1","severity":"1","condition":{"script":{"source":"false","lang":"painless"}},"actions":[]}}],"last_update_time":1,"data_sources":{"query_index":".opensearch-alerting-queries"},"owner":"alerting","ui_metadata":{"x":1}}"#;

#[test]
fn monitors() {
    let c = os_ctx(false);
    let ch = choices();
    let v = with_defaults(
        &monitor::sections(&c, true, &ch),
        json!({"name": "m", "triggers": "[]"}),
    );
    let p = monitor::plan(&c, None, &v).unwrap();
    assert_eq!(
        (p.reqs[0].method, p.reqs[0].path.as_str()),
        ("POST", "/_plugins/_alerting/monitors")
    );
    assert_eq!(p.id_from, Some(0));
    let b = p.reqs[0].body.as_ref().unwrap();
    assert_eq!(b.get("type").unwrap().text(), "monitor");
    assert_eq!(
        b.get("schedule").unwrap().compact(),
        r#"{"period":{"interval":1,"unit":"MINUTES"}}"#
    );
    for (k, x, says) in [
        ("inputs_query", json!("{}"), "lista JSON"),
        ("monitor_type", json!("composite"), "tipo de monitor"),
        ("m_interval", json!(0), "entre 1"),
        ("name", json!(""), "nombre"),
    ] {
        let mut v = v.clone();
        v.values.insert(k.into(), x);
        let e = err(monitor::plan(&c, None, &v));
        assert!(e.contains(says), "{k}: {e}");
    }
    // Open Distro: no monitor type, its own prefix.
    let od = os_ctx(true);
    let fields = monitor::sections(&od, true, &ch);
    assert!(!fields
        .iter()
        .flat_map(|s| s.fields.iter())
        .any(|f| f.key == "monitor_type"));
    let odv = with_defaults(&fields, json!({"name": "m"}));
    let p = monitor::plan(&od, None, &odv).unwrap();
    assert_eq!(p.reqs[0].path, "/_opendistro/_alerting/monitors");
    assert!(p.reqs[0]
        .body
        .as_ref()
        .unwrap()
        .get("monitor_type")
        .is_none());

    let cur = monitor::Cur {
        id: "Ab1".into(),
        monitor: j(MONITOR),
        seq: "4".into(),
        term: "1".into(),
    };
    let base = monitor::values_of(&c, &cur);
    let edit = |change: Value| {
        let mut v = FormValues {
            values: base.clone(),
            lists: BTreeMap::new(),
        };
        for (k, x) in map(change) {
            v.values.insert(k, x);
        }
        monitor::plan(&c, Some(&cur), &v)
    };
    assert!(edit(json!({})).unwrap().reqs.is_empty());
    let p =
        edit(json!({"m_schedule_kind": "cron", "m_cron": "0 */6 * * *", "enabled": true})).unwrap();
    assert_eq!(
        p.reqs[0].path,
        "/_plugins/_alerting/monitors/Ab1?if_seq_no=4&if_primary_term=1"
    );
    let b = p.reqs[0].body.as_ref().unwrap();
    // What the form doesn't show goes back; what the cluster sets doesn't.
    assert_eq!(b.get("ui_metadata").unwrap().compact(), r#"{"x":1}"#);
    assert!(b.get("data_sources").is_some() && b.get("owner").is_some());
    assert!(b.get("user").is_none() && b.get("last_update_time").is_none());
    assert_eq!(
        b.at(&["triggers"]).unwrap().compact(),
        j(MONITOR).get("triggers").unwrap().compact()
    );
    // Other inputs: the visual editor's state goes.
    let p = edit(
        json!({"inputs_query": "[{\"search\": {\"indices\": [\"j\"], \"query\": {\"size\": 0}}}]"}),
    )
    .unwrap();
    assert!(p.reqs[0]
        .body
        .as_ref()
        .unwrap()
        .get("ui_metadata")
        .is_none());
    assert!(p.render().contains("ui_metadata"));
    // A monitor another plugin manages is refused.
    let other = monitor::Cur {
        monitor: j(&MONITOR.replace(r#""owner":"alerting""#, r#""owner":"security_analytics""#)),
        ..cur.clone()
    };
    assert!(monitor::plan(
        &c,
        Some(&other),
        &FormValues {
            values: monitor::values_of(&c, &other),
            lists: BTreeMap::new()
        }
    )
    .is_err());
    let d = monitor::delete_plan("_plugins", &cur).unwrap().render();
    assert!(d.contains("DELETE /_plugins/_alerting/monitors/Ab1\n"));
    // Nor is it deleted from here (the detector would lose its monitor),
    // and the tree marks it as not the user's.
    let e = err(monitor::delete_plan("_plugins", &other));
    assert!(
        e.contains("security_analytics") && e.contains("se borra desde ese plugin"),
        "{e}"
    );
    assert!(jobs::os_monitor_job("Ab1", &other.monitor).system);
    assert!(!jobs::os_monitor_job("Ab1", &cur.monitor).system);
    assert_eq!(
        monitor::channels_text(&ch),
        json!("ch1: ch1"),
        "the channels' ids for the actions"
    );
}

#[test]
fn each_monitor_type_has_its_own_inputs_and_triggers() {
    let c = os_ctx(false);
    let ch = choices();
    let sections = monitor::sections(&c, true, &ch);
    let fields: Vec<&FormField> = sections.iter().flat_map(|s| s.fields.iter()).collect();
    for (key, types) in [
        ("inputs_query", vec!["query_level_monitor"]),
        ("inputs_bucket", vec!["bucket_level_monitor"]),
        ("inputs_doc", vec!["doc_level_monitor"]),
        ("inputs_cluster", vec!["cluster_metrics_monitor"]),
        (
            "triggers_query",
            vec!["query_level_monitor", "cluster_metrics_monitor"],
        ),
        ("triggers_bucket", vec!["bucket_level_monitor"]),
        ("triggers_doc", vec!["doc_level_monitor"]),
    ] {
        let f = fields.iter().find(|f| f.key == key).unwrap();
        let want: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        assert_eq!(f.when, Some(("monitor_type".into(), want)), "{key}");
    }
    assert!(!fields
        .iter()
        .any(|f| f.key == "inputs" || f.key == "triggers"));
    // A document level monitor sends its own inputs and triggers.
    let v = with_defaults(
        &sections,
        json!({"name": "d", "monitor_type": "doc_level_monitor",
               "triggers_doc": "[{\"document_level_trigger\": {\"name\": \"t\", \"severity\": \"1\", \"condition\": {\"script\": {\"source\": \"query[name=errores]\", \"lang\": \"painless\"}}, \"actions\": []}}]"}),
    );
    let p = monitor::plan(&c, None, &v).unwrap();
    let b = p.reqs[0].body.as_ref().unwrap();
    assert_eq!(b.get("monitor_type").unwrap().text(), "doc_level_monitor");
    assert!(b
        .at(&["inputs"])
        .unwrap()
        .compact()
        .contains("doc_level_input"));
    assert!(b
        .at(&["triggers"])
        .unwrap()
        .compact()
        .contains("document_level_trigger"));
    // A cluster metrics monitor: the uri input and per query triggers.
    let v = with_defaults(
        &sections,
        json!({"name": "c", "monitor_type": "cluster_metrics_monitor"}),
    );
    let b = monitor::plan(&c, None, &v).unwrap().reqs[0]
        .body
        .clone()
        .unwrap();
    assert!(b
        .at(&["inputs"])
        .unwrap()
        .compact()
        .contains("CLUSTER_HEALTH"));
    // A caller that sends `inputs` without the form's keys is understood.
    let v = fv(json!({"name": "q", "monitor_type": "bucket_level_monitor",
                      "m_schedule_kind": "period", "m_interval": 5, "m_interval_unit": "minutes",
                      "inputs": "[{\"search\": {\"indices\": [\"x\"], \"query\": {\"size\": 0}}}]"}));
    let b = monitor::plan(&c, None, &v).unwrap().reqs[0]
        .body
        .clone()
        .unwrap();
    assert!(b.at(&["inputs"]).unwrap().compact().contains("\"x\""));
    // An existing monitor's values go to its type's fields.
    let cur = monitor::Cur {
        id: "Ab1".into(),
        monitor: j(&MONITOR.replace("query_level_monitor", "bucket_level_monitor")),
        seq: "4".into(),
        term: "1".into(),
    };
    let v = monitor::values_of(&c, &cur);
    assert!(v.contains_key("inputs_bucket") && v.contains_key("triggers_bucket"));
    assert!(!v.contains_key("inputs_query"));
    let od = monitor::values_of(&os_ctx(true), &cur);
    assert!(od.contains_key("inputs") && od.contains_key("triggers"));
}

// ---------------------------------------------------------------------------
// Snapshot management
// ---------------------------------------------------------------------------

const SM: &str = r#"{"name":"diaria","description":"d","schema_version":21,"creation":{"schedule":{"cron":{"expression":"0 8 * * *","timezone":"UTC"}},"time_limit":"1h"},"deletion":{"schedule":{"cron":{"expression":"0 1 * * *","timezone":"UTC"}},"condition":{"max_age":"7d","max_count":5,"min_count":1}},"snapshot_config":{"repository":"repo1","indices":"a*","include_global_state":"false"},"notification":{"channel":{"id":"ch1"},"conditions":{"creation":false,"deletion":false,"failure":true,"time_limit_exceeded":false}},"schedule":{"interval":{"start_time":1,"period":1,"unit":"Minutes"}},"enabled":true,"last_updated_time":1,"enabled_time":1}"#;

#[test]
fn sm_policies() {
    let c = os_ctx(false);
    let ch = choices();
    let v = with_defaults(
        &sm::sections(),
        json!({"id": "diaria", "creation_cron": "0 8 * * *", "repository": "repo1", "enabled": false}),
    );
    let p = sm::plan(&c, None, &v, &ch).unwrap();
    assert_eq!(p.reqs[0].path, "/_plugins/_sm/policies/diaria");
    assert_eq!(
        p.reqs[0].body.as_ref().unwrap().compact(),
        r#"{"description":"","creation":{"schedule":{"cron":{"expression":"0 8 * * *","timezone":"UTC"}}},"snapshot_config":{"repository":"repo1","include_global_state":true,"partial":false,"ignore_unavailable":false},"enabled":false}"#
    );
    for (fields, says) in [
        (json!({"repository": "nope"}), "no existe"),
        (json!({"channel": "nope"}), "no existe"),
        (json!({"deletion": true}), "edad máxima o una cantidad"),
        (json!({"creation_cron": "0 0 8 * * ?"}), "expresión cron"),
        (json!({"creation_time_limit": "una hora"}), "tiempo válido"),
    ] {
        let mut v = v.clone();
        for (k, x) in map(fields.clone()) {
            v.values.insert(k, x);
        }
        let e = err(sm::plan(&c, None, &v, &ch));
        assert!(e.contains(says), "{fields}: {e}");
    }

    let cur = sm::Cur {
        name: "diaria".into(),
        policy: j(SM),
        seq: "9".into(),
        term: "3".into(),
    };
    let base = sm::values_of(&cur);
    assert_eq!(base["include_global_state"], json!(false));
    assert_eq!(base["notify_on"], json!(["failure"]));
    let edit = |change: Value| {
        let mut v = FormValues {
            values: base.clone(),
            lists: BTreeMap::new(),
        };
        for (k, x) in map(change) {
            v.values.insert(k, x);
        }
        sm::plan(&c, Some(&cur), &v, &ch).unwrap()
    };
    assert!(
        edit(json!({})).reqs.is_empty(),
        "{}",
        edit(json!({})).render()
    );
    let p = edit(json!({"max_count": 10, "enabled": false}));
    let paths: Vec<(&str, &str)> = p.reqs.iter().map(|r| (r.method, r.path.as_str())).collect();
    assert_eq!(
        paths,
        [
            (
                "PUT",
                "/_plugins/_sm/policies/diaria?if_seq_no=9&if_primary_term=3"
            ),
            ("POST", "/_plugins/_sm/policies/diaria/_stop")
        ]
    );
    // An update doesn't carry "enabled" (_start / _stop do).
    assert!(p.reqs[0].body.as_ref().unwrap().get("enabled").is_none());
    let p = edit(json!({"deletion": false, "channel": ""}));
    let b = p.reqs[0].body.as_ref().unwrap();
    assert!(b.get("deletion").is_none() && b.get("notification").is_none());
    let p = edit(json!({"id": "diaria2"}));
    let paths: Vec<(&str, &str)> = p.reqs.iter().map(|r| (r.method, r.path.as_str())).collect();
    assert_eq!(
        paths,
        [
            ("POST", "/_plugins/_sm/policies/diaria2"),
            ("DELETE", "/_plugins/_sm/policies/diaria")
        ]
    );
    assert!(sm::delete_plan("_plugins", &cur)
        .render()
        .contains("DELETE /_plugins/_sm/policies/diaria\n"));
}

// ---------------------------------------------------------------------------
// Jobs of the new types
// ---------------------------------------------------------------------------

#[test]
fn snapshot_policies_as_jobs() {
    let p = j(
        r#"{"version":2,"policy":{"name":"<n>","schedule":"0 30 1 * * ?","repository":"repo1"},"last_success":{"snapshot_name":"s1","start_time":1794520297365,"time":1794520297473},"last_failure":{"snapshot_name":"s0","time":1794500000000,"details":"x"},"next_execution_millis":1794533400000}"#,
    );
    let job = jobs::slm_job("noche", &p, true, true);
    assert_eq!(job.id, "slm:noche");
    assert_eq!(job.last_outcome, Some(dbine_driver::JobOutcome::Succeeded));
    assert_eq!(job.last_duration_ms, Some(108));
    assert_eq!(job.schedule.as_deref(), Some("cron 0 30 1 * * ?"));
    assert_eq!(job.next_run.as_deref(), Some("2026-11-13T01:30:00Z"));
    assert!(job.can_run && !job.can_enable && job.has_history);
    let stopped = jobs::slm_job("noche", &p, false, false);
    assert!(!stopped.enabled && !stopped.has_history);
    let d = jobs::slm_detail("noche", &p, false, true);
    assert_eq!(d.steps[0].name, "Snapshot");
    assert_eq!(d.notes, [jobs::SLM_STOPPED]);
    let run = jobs::slm_run(
        None,
        &j(
            r#"{"@timestamp":1794520297473,"policy":"noche","snapshot_name":"s1","operation":"DELETE","success":false,"error_details":"boom"}"#,
        ),
    );
    assert_eq!(run.outcome, dbine_driver::JobOutcome::Failed);
    assert_eq!(
        run.message.as_deref(),
        Some("Borrado del snapshot s1: boom")
    );

    let sm_policy = j(SM);
    let explain = j(
        r#"{"name":"diaria","creation":{"current_state":"CREATION_START","trigger":{"time":1794560400000},"latest_execution":{"status":"FAILED","start_time":1794474000000,"end_time":1794474005000,"info":{"message":"m","cause":"c"}}}}"#,
    );
    let job = jobs::sm_job(&sm_policy, Some(&explain));
    assert_eq!(job.id, "sm:diaria");
    assert_eq!(job.last_outcome, Some(dbine_driver::JobOutcome::Failed));
    assert_eq!(job.last_duration_ms, Some(5000));
    assert_eq!(job.schedule.as_deref(), Some("cron 0 8 * * * (UTC)"));
    assert!(!job.can_run && job.can_enable);
    let d = jobs::sm_detail(&sm_policy, Some(&explain));
    assert_eq!(d.steps.len(), 2);
    let s = jobs::sm_script("_plugins", &sm_policy);
    assert!(s.starts_with("POST /_plugins/_sm/policies/diaria\n"), "{s}");
    assert!(!s.contains("schema_version") && !s.contains("last_updated_time"));
    assert_eq!(jobs::parse_id("slm:noche"), Some((JobKind::Slm, "noche")));
    assert_eq!(jobs::parse_id("sm:diaria"), Some((JobKind::Sm, "diaria")));
    assert_eq!(jobs::parse_id("sm:*"), None);
}

// ---------------------------------------------------------------------------
// ISM and ILM policies
// ---------------------------------------------------------------------------

#[test]
fn lifecycle_kinds() {
    assert_eq!(check_kind("ism").unwrap(), Kind::Lifecycle);
    assert_eq!(check_kind("ilm").unwrap(), Kind::Lifecycle);
    let es = info_of(&es_ctx());
    let keys: Vec<&str> = es.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "ilm"]);
    let os = info_of(&os_ctx(false));
    let keys: Vec<&str> = os.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "ism"]);
    let mut c = es_ctx();
    c.lifecycle = false;
    assert_eq!(info_of(&c).kinds.len(), 1);
    assert!(lifecycle_ready(&c, false).is_err());
    c.lifecycle = true;
    c.manage_lifecycle = false;
    assert!(lifecycle_ready(&c, false).is_ok() && lifecycle_ready(&c, true).is_err());
    assert!(!info_of(&c).kinds[1].can_create);
}

#[test]
fn flags_and_actions_per_cluster() {
    // Elasticsearch: watches start at an action; watches and SLM keep a
    // history; a stopped transform resets, a watch's actions are acked.
    let es = info_of(&es_ctx());
    let job = &es.kinds[0];
    assert!(job.run_from_step && job.clear_history);
    let keys: Vec<&str> = job.actions.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["reset", "ack"]);
    assert!(es.kinds[1].actions.is_empty() && !es.kinds[1].run_from_step);
    // Without Watcher (basic license): no start at a step; SLM's history.
    let mut c = es_ctx();
    c.types = vec![Type::Transform, Type::Slm];
    c.manage = c.types.clone();
    let job = &info_of(&c).kinds[0];
    assert!(!job.run_from_step && job.clear_history);
    assert_eq!(job.actions.len(), 1);
    // Before 8.1 there's no _reset; without manage_transform, no action.
    c.reset = false;
    assert!(info_of(&c).kinds[0].actions.is_empty());
    let mut c = es_ctx();
    c.manage = vec![Type::Slm];
    assert!(info_of(&c).kinds[0].actions.is_empty());
    // Watcher there but without manage_watcher (monitor_watcher only):
    // `_execute` would be refused, so no start at a step; the history of
    // the watches is still there to clear.
    let mut c = es_ctx();
    c.manage = vec![Type::Transform, Type::Slm];
    let job = &info_of(&c).kinds[0];
    assert!(!job.run_from_step && job.clear_history);
    let keys: Vec<&str> = job.actions.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys, ["reset"]);
    // OpenSearch and Open Distro: the switch and "run now" are all there is.
    for c in [os_ctx(false), os_ctx(true)] {
        let job = &info_of(&c).kinds[0];
        assert!(!job.run_from_step && !job.clear_history && job.actions.is_empty());
    }
}

fn a_job(id: &str, enabled: bool, system: bool) -> ServerJob {
    ServerJob {
        id: id.into(),
        name: id.into(),
        kind: String::new(),
        group: None,
        description: None,
        owner: None,
        enabled,
        schedule: None,
        running: false,
        last_outcome: None,
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: true,
        can_stop: false,
        can_enable: true,
        has_history: false,
        has_script: true,
        system,
    }
}

#[test]
fn each_job_says_what_it_may_do() {
    let c = es_ctx();
    let ackable = BTreeSet::from(["w1".to_string()]);
    let item =
        |id: &str, enabled: bool, system: bool| item_of(&c, a_job(id, enabled, system), &ackable);
    // A stopped transform resets; a started one doesn't.
    assert_eq!(
        item("transform:t", false, false).actions,
        Some(vec!["reset".into()])
    );
    assert_eq!(item("transform:t", true, false).actions, Some(vec![]));
    // A watch with an action waiting is acked; another isn't.
    assert_eq!(
        item("watch:w1", true, false).actions,
        Some(vec!["ack".into()])
    );
    assert_eq!(item("watch:w2", true, false).actions, Some(vec![]));
    assert_eq!(item("slm:p", true, false).actions, Some(vec![]));
    // Elasticsearch's own watches are neither changed nor deleted.
    let sys = item("watch:w1", true, true);
    assert_eq!((sys.can_edit, sys.can_delete), (Some(false), Some(false)));
    assert_eq!(sys.actions, Some(vec![]));
    // Elastic Cloud's snapshot policy is changed, not deleted.
    let cloud = item("slm:cloud-snapshot-policy", true, true);
    assert_eq!((cloud.can_edit, cloud.can_delete), (None, Some(false)));
    let own = item("slm:p", true, false);
    assert_eq!((own.can_edit, own.can_delete), (None, None));
    // OpenSearch: another plugin's monitor; no actions at all.
    let c = os_ctx(false);
    let m = item_of(&c, a_job("monitor:m", true, true), &BTreeSet::new());
    assert_eq!(
        (m.can_edit, m.can_delete, m.actions),
        (Some(false), Some(false), None)
    );
    let m = item_of(&c, a_job("monitor:m", true, false), &BTreeSet::new());
    assert_eq!((m.can_edit, m.can_delete), (None, None));
}

#[test]
fn a_transform_resets_only_stopped() {
    let def = j(r#"{"id":"t1","source":{"index":["a"]},"dest":{"index":"b"},"pivot":{}}"#);
    let cur = |state: &str, managed: bool| es_transform::Cur {
        id: "t1".into(),
        def: def.clone(),
        state: state.into(),
        managed,
    };
    let p = es_transform::reset_plan(&cur("stopped", false)).unwrap();
    let r = p.render();
    assert!(r.ends_with("POST /_transform/t1/_reset\n"), "{r}");
    assert!(r.contains("índice de destino"), "{r}");
    assert!(!r.contains("Lo administra Elastic"));
    assert!(es_transform::reset_plan(&cur("stopped", true))
        .unwrap()
        .render()
        .contains("Lo administra Elastic"));
    for state in ["started", "indexing", "failed"] {
        let e = err(es_transform::reset_plan(&cur(state, false)));
        assert!(e.contains("detenerlo") && e.contains(state), "{e}");
    }
}

#[test]
fn a_watch_is_acked_when_an_action_waits() {
    let mut cur = watch_cur(WATCH, true);
    assert!(err(watch::ack_plan(&cur)).contains("Ninguna acción"));
    cur.ackable = true;
    let r = watch::ack_plan(&cur).unwrap().render();
    assert!(r.ends_with("PUT /_watcher/watch/errores/_ack\n"), "{r}");
    let hit = |state: &str| {
        j(&format!(
            r#"{{"_id":"w","status":{{"actions":{{"a":{{"ack":{{"state":"awaits_successful_execution"}}}},"b":{{"ack":{{"state":"{state}"}}}}}}}}}}"#
        ))
    };
    assert!(jobs::watch_ackable(&hit("ackable")));
    assert!(!jobs::watch_ackable(&hit("acked")));
    assert!(!jobs::watch_ackable(&j(r#"{"_id":"w"}"#)));
    assert!(jobs::system_watch(&j(
        r#"{"metadata":{"xpack":{"type":"monitoring"}}}"#
    )));
    assert!(!jobs::system_watch(&j(r#"{"metadata":{"name":"x"}}"#)));
}

const ISM: &str = r#"{"policy_id":"p","description":"d","last_updated_time":1,"schema_version":21,"error_notification":null,"default_state":"hot","states":[{"name":"hot","actions":[],"transitions":[{"state_name":"delete","conditions":{"min_index_age":"30d"}}]},{"name":"delete","actions":[{"retry":{"count":3,"backoff":"exponential","delay":"1m"},"delete":{}}],"transitions":[]}],"ism_template":[{"index_patterns":["logs-*"],"priority":10,"last_updated_time":1}]}"#;

#[test]
fn ism_policies() {
    let c = os_ctx(false);
    let v = with_defaults(
        &lifecycle::ism_sections(),
        json!({"id": "p", "default_state": "caliente"}),
    );
    let p = lifecycle::ism_plan(&c, None, &v).unwrap();
    assert_eq!(
        (p.reqs[0].method, p.reqs[0].path.as_str()),
        ("PUT", "/_plugins/_ism/policies/p")
    );
    assert_eq!(p.id.as_deref(), Some("p"));
    let body = p.reqs[0].body.as_ref().unwrap();
    assert_eq!(
        body.at(&["policy", "default_state"]).unwrap().text(),
        "caliente"
    );
    for (k, x, says) in [
        ("default_state", json!("frio"), "no es uno de los estados"),
        ("states", json!("{}"), "lista JSON"),
        ("ism_template", json!("[1"), "JSON válido"),
        ("id", json!("_p"), "Id"),
    ] {
        let mut v = v.clone();
        v.values.insert(k.into(), x);
        let e = err(lifecycle::ism_plan(&c, None, &v));
        assert!(e.contains(says), "{k}: {e}");
    }
    let cur = lifecycle::Cur {
        id: "p".into(),
        policy: j(ISM),
        seq: "5".into(),
        term: "2".into(),
        in_use: Vec::new(),
    };
    let base = lifecycle::ism_values(&cur);
    // The time the cluster stamps on a template isn't shown or sent back.
    assert!(!base["ism_template"]
        .as_str()
        .unwrap()
        .contains("last_updated_time"));
    let edit = |change: Value| {
        let mut v = FormValues {
            values: base.clone(),
            lists: BTreeMap::new(),
        };
        for (k, x) in map(change) {
            v.values.insert(k, x);
        }
        lifecycle::ism_plan(&c, Some(&cur), &v).unwrap()
    };
    assert!(edit(json!({})).reqs.is_empty());
    let p = edit(json!({"description": "otra"}));
    assert_eq!(
        p.reqs[0].path,
        "/_plugins/_ism/policies/p?if_seq_no=5&if_primary_term=2"
    );
    assert!(p.render().contains(lifecycle::ISM_NOTE));
    // With a template: the new policy without it (the cluster checks the
    // states before anything is deleted), the old one deleted, then the
    // template on the new one with the first PUT's seq_no / primary_term.
    // The policy is never missing.
    let p = edit(json!({"id": "p2"}));
    let paths: Vec<(&str, &str)> = p.reqs.iter().map(|r| (r.method, r.path.as_str())).collect();
    assert_eq!(
        paths,
        [
            ("PUT", "/_plugins/_ism/policies/p2"),
            ("DELETE", "/_plugins/_ism/policies/p"),
            ("PUT", "/_plugins/_ism/policies/p2")
        ]
    );
    let first = p.reqs[0].body.as_ref().unwrap();
    assert!(first.at(&["policy", "ism_template"]).is_none());
    assert!(first.at(&["policy", "states"]).is_some());
    assert_eq!(
        first.at(&["policy", "default_state"]).unwrap().text(),
        "hot"
    );
    let last = &p.reqs[2];
    assert_eq!(last.guard_from, Some(0));
    assert!(last
        .body
        .as_ref()
        .unwrap()
        .at(&["policy", "ism_template"])
        .is_some());
    assert!(last
        .if_fails
        .as_deref()
        .unwrap()
        .contains("sin su plantilla"));
    assert_eq!(p.reqs[0].guard_from, None);
    let shown = p.render();
    assert!(shown.contains("if_seq_no / if_primary_term de la respuesta del primer PUT"));
    // The guard comes from the first PUT's answer, numbers only.
    assert_eq!(
        guarded(
            &last.path,
            &j(r#"{"_id":"p2","_seq_no":3650,"_primary_term":18}"#)
        )
        .as_deref(),
        Some("/_plugins/_ism/policies/p2?if_seq_no=3650&if_primary_term=18")
    );
    assert_eq!(guarded(&last.path, &j(r#"{"_id":"p2"}"#)), None);
    assert_eq!(
        guarded(&last.path, &j(r#"{"_seq_no":"1&x=2","_primary_term":1}"#)),
        None
    );
    let p = edit(json!({"id": "p2", "ism_template": ""}));
    let paths: Vec<(&str, &str)> = p.reqs.iter().map(|r| (r.method, r.path.as_str())).collect();
    assert_eq!(
        paths,
        [
            ("PUT", "/_plugins/_ism/policies/p2"),
            ("DELETE", "/_plugins/_ism/policies/p")
        ]
    );
    // Open Distro takes one template, as an object.
    let od = os_ctx(true);
    let mut v = FormValues {
        values: base.clone(),
        lists: BTreeMap::new(),
    };
    let p = lifecycle::ism_plan(&od, Some(&cur), &v).unwrap();
    assert!(p.reqs.is_empty(), "{}", p.render());
    v.values.insert("description".into(), json!("x"));
    let p = lifecycle::ism_plan(&od, Some(&cur), &v).unwrap();
    assert_eq!(
        p.reqs[0]
            .body
            .as_ref()
            .unwrap()
            .at(&["policy", "ism_template"])
            .unwrap()
            .compact(),
        r#"{"index_patterns":["logs-*"],"priority":10}"#
    );
    v.values.insert(
        "ism_template".into(),
        json!("[{\"index_patterns\": [\"a\"]}, {\"index_patterns\": [\"b\"]}]"),
    );
    assert!(err(lifecycle::ism_plan(&od, Some(&cur), &v)).contains("una sola plantilla"));
    let d = lifecycle::delete_plan(&os_ctx(true), &cur).unwrap();
    assert_eq!(d.reqs[0].path, "/_opendistro/_ism/policies/p");
}

#[test]
fn ilm_policies() {
    let v = with_defaults(&lifecycle::ilm_sections(), json!({"id": "logs"}));
    let p = lifecycle::ilm_plan(None, &v).unwrap();
    assert_eq!(p.reqs[0].path, "/_ilm/policy/logs");
    assert!(p.reqs[0]
        .body
        .as_ref()
        .unwrap()
        .at(&["policy", "phases", "hot"])
        .is_some());
    let mut bad_v = v.clone();
    bad_v.values.insert("phases".into(), json!("[]"));
    assert!(err(lifecycle::ilm_plan(None, &bad_v)).contains("objeto JSON"));
    let cur = lifecycle::Cur {
        id: "logs".into(),
        policy: j(
            r#"{"phases":{"delete":{"min_age":"30d","actions":{"delete":{"delete_searchable_snapshot":true}}}},"_meta":{"description":"x"}}"#,
        ),
        seq: String::new(),
        term: String::new(),
        in_use: vec!["idx1".into(), "tmpl".into()],
    };
    let base = lifecycle::ilm_values(&cur);
    assert_eq!(base["in_use"], json!("idx1\ntmpl"));
    let edit = |change: Value| {
        let mut v = FormValues {
            values: base.clone(),
            lists: BTreeMap::new(),
        };
        for (k, x) in map(change) {
            v.values.insert(k, x);
        }
        lifecycle::ilm_plan(Some(&cur), &v)
    };
    assert!(edit(json!({})).unwrap().reqs.is_empty());
    let p = edit(json!({"meta": ""})).unwrap();
    assert_eq!(p.reqs[0].path, "/_ilm/policy/logs");
    assert!(p.reqs[0]
        .body
        .as_ref()
        .unwrap()
        .at(&["policy", "_meta"])
        .is_none());
    // A policy Elastic manages is read only.
    let managed = lifecycle::Cur {
        policy: j(r#"{"phases":{},"_meta":{"managed":true}}"#),
        ..cur.clone()
    };
    assert!(lifecycle::ilm_plan(
        Some(&managed),
        &FormValues {
            values: lifecycle::ilm_values(&managed),
            lists: BTreeMap::new()
        }
    )
    .is_err());
    assert!(lifecycle::delete_plan(&es_ctx(), &managed).is_err());
    let d = lifecycle::delete_plan(&es_ctx(), &cur).unwrap().render();
    assert!(d.contains("DELETE /_ilm/policy/logs\n"), "{d}");
}
