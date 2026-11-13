//! Unit tests of the scheduler designer: every script generator (create,
//! each change, delete), the escaping and the checks.

use super::*;
use dbine_driver::{FormValues, ObjectForm};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn values(v: Value) -> FormValues {
    FormValues {
        values: v
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        lists: BTreeMap::new(),
    }
}

fn with_list(mut f: FormValues, list: &str, rows: Value) -> FormValues {
    let rows = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r.as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .collect();
    f.lists.insert(list.into(), rows);
    f
}

/// What the designer sends back for a form as it came.
fn as_sent(form: &ObjectForm) -> FormValues {
    let mut values = form.values.clone();
    for f in form.sections.iter().flat_map(|s| s.fields.iter()) {
        if let Some(d) = &f.default {
            values.entry(f.key.clone()).or_insert_with(|| d.clone());
        }
    }
    let lists = form
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l| (l.key.clone(), l.rows.clone()))
        .collect();
    FormValues { values, lists }
}

fn err(r: dbine_driver::Result<impl std::fmt::Debug>) -> String {
    match r {
        Err(dbine_driver::Error::Query(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn field<'a>(form: &'a ObjectForm, key: &str) -> &'a dbine_driver::FormField {
    form.sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|f| f.key == key)
        .unwrap_or_else(|| panic!("field {key}"))
}

// ---------------------------------------------------------------- shared

#[test]
fn kinds_and_checks() {
    assert!(check_kind("job").is_ok());
    assert!(check_kind(" job ").is_ok());
    assert!(err(check_kind("schedule")).contains("no es un tipo"));
    assert!(check_line("a b", "Nombre").is_ok());
    assert!(err(check_line("a\nb", "Nombre")).contains("saltos de línea"));
    assert_eq!(shown("a\nb\tc"), "a b c");
    assert_eq!(shown(&"x".repeat(200)).len(), 80);
}

#[test]
fn durations_and_crons() {
    let flux = &["ns", "us", "µs", "ms", "s", "m", "h", "d", "w", "mo", "y"];
    for ok in ["1h", "30s", "1h30m", "2w", "1mo", "15ms"] {
        assert!(duration_ok(ok, flux, false), "{ok}");
    }
    for bad in [
        "", "1", "h", "1x", "-1h", "1h;", "1 h", "1h)", "1.5h", "1h\n2m",
    ] {
        assert!(!duration_ok(bad, flux, false), "{bad:?}");
    }
    let influxql = &["ns", "u", "µ", "ms", "s", "m", "h", "d", "w"];
    assert!(duration_ok("1u", influxql, false));
    assert!(!duration_ok("1mo", influxql, false));
    let human = &["s", "m", "h", "minutes", "d"];
    assert!(duration_ok("1h 30m", human, true));
    assert!(duration_ok("10 minutes", human, true));
    assert!(!duration_ok("10 parsecs", human, true));

    assert!(cron_ok("0 3 * * *", 5, 7, true));
    assert!(cron_ok("@hourly", 5, 7, true));
    assert!(cron_ok("0 0 3 * * MON-FRI", 6, 7, true));
    assert!(cron_ok("*/5 * * * * ?", 6, 7, true));
    assert!(!cron_ok("0 3 * * *", 6, 7, true));
    assert!(!cron_ok("@", 5, 7, true));
    assert!(!cron_ok("0 3 * * * '", 5, 7, true));
    assert!(!cron_ok("0 3 * * *\"; x", 5, 7, true));
}

#[test]
fn requests_render_as_comments() {
    let r = Req::new(
        "POST",
        format!("/api/v2/tasks/{NEW_ID}/labels"),
        Some(json!({ "flux": "a\nb", "x": "it's" })),
    );
    let url = r.url("http://h:1", Some("0123456789abcdef")).unwrap();
    assert_eq!(
        url.as_str(),
        "http://h:1/api/v2/tasks/0123456789abcdef/labels"
    );
    let q = Req::new("POST", "/api/v3/x", None)
        .query("db", "a b&c")
        .query("trigger_name", "t=1");
    assert_eq!(
        q.url("http://h", None).unwrap().as_str(),
        "http://h/api/v3/x?db=a+b%26c&trigger_name=t%3D1"
    );
    let script = render(
        "//",
        &["Crea «x\n// y»".to_string()],
        &[r.clone(), q.clone()],
    );
    assert_eq!(
        script,
        "// Crea «x // y»\n//\n// POST /api/v2/tasks/{id}/labels\n// {\n//   \"flux\": \"a\\nb\",\n//   \"x\": \"it's\"\n// }\n//\n// POST /api/v3/x?db=a+b%26c&trigger_name=t%3D1\n"
    );
    // Every line is a comment: nothing in a value runs.
    assert!(script.lines().all(|l| l.starts_with("//")));
}

// ---------------------------------------------------------------- 2.x

mod tasks {
    use super::*;
    use crate::scheduler::task::*;

    fn task(flux: &str) -> Task {
        task_of(&json!({
            "id": "0123456789abcdef", "orgID": "aaaabbbbccccdddd", "org": "dbine",
            "ownerID": "1111222233334444", "name": "nightly", "description": "probe",
            "status": "active", "flux": flux, "every": "1h", "offset": "5m",
            "labels": [{"id": "1000000000000001", "name": "prod"}],
            "createdAt": "2026-11-12T13:02:04Z", "latestCompleted": "2026-11-12T13:02:09Z",
            "lastRunStatus": "failed", "lastRunError": "bucket not found"
        }))
    }

    const FLUX: &str = "import \"array\"\n\noption task = {name: \"nightly\", every: 1h, offset: 5m, concurrency: 1}\n\narray.from(rows: [{_value: 1}])\n  |> yield()\n";

    fn labels() -> Vec<(String, String)> {
        vec![
            ("1000000000000001".into(), "prod".into()),
            ("1000000000000002".into(), "etl".into()),
        ]
    }

    #[test]
    fn options_are_found() {
        let o = find_options(FLUX).unwrap();
        assert_eq!(&FLUX[o.start..o.start + 6], "option");
        assert!(FLUX[..o.end].ends_with("concurrency: 1}"));
        assert_eq!(
            o.fields,
            [
                ("name".to_string(), "\"nightly\"".to_string()),
                ("every".into(), "1h".into()),
                ("offset".into(), "5m".into()),
                ("concurrency".into(), "1".into()),
            ]
        );
        assert_eq!(
            body_of(FLUX, &o),
            "import \"array\"\n\narray.from(rows: [{_value: 1}])\n  |> yield()"
        );
        // Strings and comments don't count; a record over several lines
        // with commas and braces inside its strings does.
        let tricky = "// option task = {name: \"no\"}\nx = \"option task = {\"\noption   task =\n  {\n    name: \"a, {b}\",\n    cron: \"0 3 * * *\",\n  }\nx";
        let o = find_options(tricky).unwrap();
        assert_eq!(
            o.fields,
            [
                ("name".to_string(), "\"a, {b}\"".to_string()),
                ("cron".into(), "\"0 3 * * *\"".into()),
            ]
        );
        assert_eq!(
            body_of(tricky, &o),
            "// option task = {name: \"no\"}\nx = \"option task = {\"\n\nx"
        );
        assert!(find_options("from(bucket: \"b\")").is_none());
        assert!(find_options("myoption task = {name: \"x\"}").is_none());
        assert!(find_options("option task = {name: }").is_none());
        assert!(find_options("option tasks = {name: \"x\"}").is_none());
    }

    #[test]
    fn the_option_goes_after_the_imports() {
        let fields = vec![
            ("name".to_string(), "\"n\"".to_string()),
            ("every".into(), "1h".into()),
        ];
        assert_eq!(
            compose("import \"array\"\n// a note\nimport \"strings\"\n\narray.from(rows: [])", &fields),
            "import \"array\"\n// a note\nimport \"strings\"\n\noption task = {name: \"n\", every: 1h}\n\narray.from(rows: [])\n"
        );
        assert_eq!(
            compose("from(bucket: \"b\")", &fields),
            "option task = {name: \"n\", every: 1h}\n\nfrom(bucket: \"b\")\n"
        );
        // What the designer shows comes back whole.
        let t = task(FLUX);
        let o = find_options(FLUX).unwrap();
        let again = compose(&body_of(FLUX, &o), &o.fields);
        assert_eq!(again, FLUX);
        assert_eq!(t.flux, FLUX);
    }

    #[test]
    fn a_new_task() {
        let v = values(json!({
            "name": "my \"task\" ${x}", "description": "", "enabled": false, "mode": "every",
            "every": "1h30m", "offset": "", "labels": ["1000000000000002"],
            "flux": "import \"array\"\narray.from(rows: [{_value: 1}])"
        }));
        let p = create_plan("dbine", &labels(), &v).unwrap();
        assert_eq!(p.reqs.len(), 2);
        assert_eq!(p.reqs[0].method, "POST");
        assert_eq!(p.reqs[0].path, "/api/v2/tasks");
        assert_eq!(
            p.reqs[0].body,
            Some(json!({
                "org": "dbine", "status": "inactive",
                "flux": "import \"array\"\n\noption task = {name: \"my \\\"task\\\" \\${x}\", every: 1h30m}\n\narray.from(rows: [{_value: 1}])\n"
            }))
        );
        assert_eq!(p.reqs[1].path, "/api/v2/tasks/{id}/labels");
        assert_eq!(p.reqs[1].body, Some(json!({"labelID": "1000000000000002"})));
        let script = p.script();
        assert!(
            script.starts_with("// Crea la tarea «my \"task\" ${x}» en la organización «dbine»"),
            "{script}"
        );
        assert!(script.contains("Etiquetas: etl."), "{script}");

        let v = values(json!({
            "name": "c", "description": "d", "mode": "cron", "cron": "0 3 * * *",
            "offset": "5m", "flux": "from(bucket: \"b\")"
        }));
        let p = create_plan("o", &[], &v).unwrap();
        assert_eq!(p.reqs.len(), 1);
        let body = p.reqs[0].body.as_ref().unwrap();
        assert_eq!(body["status"], "active");
        assert_eq!(body["description"], "d");
        assert_eq!(
            body["flux"],
            "option task = {name: \"c\", cron: \"0 3 * * *\", offset: 5m}\n\nfrom(bucket: \"b\")\n"
        );
    }

    #[test]
    fn a_new_task_is_checked() {
        let base =
            json!({"name": "n", "mode": "every", "every": "1h", "flux": "from(bucket: \"b\")"});
        let with = |k: &str, v: Value| {
            let mut b = base.clone();
            b[k] = v;
            values(b)
        };
        let cases = [
            (with("name", json!("")), "necesita un nombre"),
            (with("name", json!("a\nb")), "saltos de línea"),
            (with("every", json!("")), "every"),
            (with("every", json!("1h; drop")), "no es una duración"),
            (with("mode", json!("sometimes")), "every o cron"),
            (with("offset", json!("5 minutes")), "desfase"),
            (with("flux", json!("  ")), "necesita su Flux"),
            (
                with(
                    "flux",
                    json!("option task = {name: \"x\", every: 1h}\nfrom(bucket: \"b\")"),
                ),
                "option task",
            ),
            (
                with("labels", json!(["9999999999999999"])),
                "no es una etiqueta",
            ),
        ];
        for (v, msg) in cases {
            let m = err(create_plan("o", &labels(), &v));
            assert!(m.contains(msg), "{msg}: {m}");
        }
        let mut cron = base.clone();
        cron["mode"] = json!("cron");
        cron["cron"] = json!("0 3 * *");
        assert!(err(create_plan("o", &[], &values(cron))).contains("expresión cron"));
    }

    #[test]
    fn the_form_of_a_task() {
        let t = task(FLUX);
        let f = form_of(Some(&t), Some(&labels()), Some("admin".into()), "dbine");
        assert!(!f.read_only);
        assert_eq!(f.values["name"], "nightly");
        assert_eq!(f.values["mode"], "every");
        assert_eq!(f.values["every"], "1h");
        assert_eq!(f.values["offset"], "5m");
        assert_eq!(f.values["enabled"], true);
        assert_eq!(f.values["labels"], json!(["1000000000000001"]));
        assert_eq!(f.values["owner"], "admin");
        assert_eq!(f.values["last_error"], "bucket not found");
        assert_eq!(
            f.values["flux"],
            "import \"array\"\n\narray.from(rows: [{_value: 1}])\n  |> yield()"
        );
        assert_eq!(f.choices["label"].len(), 2);
        assert!(field(&f, "org").read_only && field(&f, "created").read_only);
        assert!(!field(&f, "every").read_only);
        assert_eq!(
            field(&f, "every").when,
            Some(("mode".into(), vec!["every".into()]))
        );
        // Sent back as it came: nothing to do.
        let p = edit_plan(&t, &labels(), &as_sent(&f)).unwrap();
        assert!(p.reqs.is_empty(), "{p:?}");
        assert!(p.script().contains("no hay cambios"));

        // A new one: defaults.
        let f = form_of(None, None, None, "dbine");
        assert_eq!(f.values["every"], "1h");
        assert_eq!(f.values["enabled"], true);
        assert_eq!(f.values["org"], "dbine");
        assert!(f.notes.iter().any(|n| n.contains("etiquetas")));
    }

    #[test]
    fn a_change_patches_only_what_changed() {
        let t = task(FLUX);
        let f = form_of(Some(&t), Some(&labels()), None, "dbine");
        let base = as_sent(&f);
        let change = |k: &str, v: Value| {
            let mut x = base.clone();
            x.values.insert(k.into(), v);
            edit_plan(&t, &labels(), &x).unwrap()
        };
        let p = change("every", json!("2h"));
        assert_eq!(p.reqs.len(), 1);
        assert_eq!(p.reqs[0].method, "PATCH");
        assert_eq!(p.reqs[0].path, "/api/v2/tasks/0123456789abcdef");
        assert_eq!(p.reqs[0].body, Some(json!({"every": "2h"})));
        assert_eq!(p.id.as_deref(), Some("0123456789abcdef"));

        let mut x = base.clone();
        x.values.insert("mode".into(), json!("cron"));
        x.values.insert("cron".into(), json!("0 3 * * *"));
        let p = edit_plan(&t, &labels(), &x).unwrap();
        assert_eq!(p.reqs[0].body, Some(json!({"cron": "0 3 * * *"})));

        assert_eq!(
            change("offset", json!("10m")).reqs[0].body,
            Some(json!({"offset": "10m"}))
        );
        assert_eq!(
            change("name", json!("renamed")).reqs[0].body,
            Some(json!({"name": "renamed"}))
        );
        assert_eq!(
            change("enabled", json!(false)).reqs[0].body,
            Some(json!({"status": "inactive"}))
        );
        assert_eq!(
            change("description", json!("new")).reqs[0].body,
            Some(json!({"description": "new"}))
        );
        // Clearing the offset or the description needs the Flux; the
        // option the task had besides stays.
        let p = change("offset", json!(""));
        assert_eq!(
            p.reqs[0].body,
            Some(
                json!({"flux": "import \"array\"\n\noption task = {name: \"nightly\", every: 1h, concurrency: 1}\n\narray.from(rows: [{_value: 1}])\n  |> yield()\n"})
            )
        );
        let p = change("description", json!(""));
        let body = p.reqs[0].body.as_ref().unwrap();
        assert_eq!(body["description"], "");
        assert_eq!(body["flux"], FLUX);
        // A new script: the whole Flux, with the options as the form says.
        let mut x = base.clone();
        x.values.insert(
            "flux".into(),
            json!("import \"array\"\narray.from(rows: [{_value: 2}])"),
        );
        x.values.insert("every".into(), json!("15m"));
        let p = edit_plan(&t, &labels(), &x).unwrap();
        assert_eq!(
            p.reqs[0].body,
            Some(
                json!({"flux": "import \"array\"\n\noption task = {name: \"nightly\", every: 15m, offset: 5m, concurrency: 1}\n\narray.from(rows: [{_value: 2}])\n"})
            )
        );
        // Labels: one added, one taken away.
        let p = change("labels", json!(["1000000000000002"]));
        assert_eq!(p.reqs.len(), 2);
        assert_eq!(
            (p.reqs[0].method, p.reqs[0].path.as_str()),
            ("POST", "/api/v2/tasks/0123456789abcdef/labels")
        );
        assert_eq!(p.reqs[0].body, Some(json!({"labelID": "1000000000000002"})));
        assert_eq!(
            (p.reqs[1].method, p.reqs[1].path.as_str()),
            (
                "DELETE",
                "/api/v2/tasks/0123456789abcdef/labels/1000000000000001"
            )
        );
        assert!(err(edit_plan(&t, &labels(), &{
            let mut x = base.clone();
            x.values.insert("labels".into(), json!(["../../orgs"]));
            x
        }))
        .contains("no es una etiqueta"));
    }

    #[test]
    fn runs_at_once_retries_and_the_other_options() {
        const MORE: &str = "import \"array\"\n\noption task = {name: \"nightly\", every: 1h, offset: 5m, concurrency: 2, retry: 3, foo: 1}\n\narray.from(rows: [{_value: 1}])\n  |> yield()\n";
        let t = task(MORE);
        let f = form_of(Some(&t), Some(&labels()), None, "dbine");
        assert_eq!(f.values["concurrency"], 2);
        assert_eq!(f.values["retry"], 3);
        assert_eq!(f.values["other_options"], "foo: 1");
        assert!(!field(&f, "concurrency").read_only && !field(&f, "retry").read_only);
        assert!(field(&f, "other_options").read_only);
        let base = as_sent(&f);
        assert!(edit_plan(&t, &labels(), &base).unwrap().reqs.is_empty());
        let change = |k: &str, v: Value| {
            let mut x = base.clone();
            x.values.insert(k.into(), v);
            edit_plan(&t, &labels(), &x)
        };
        // The API has no field for them: the whole Flux, the rest kept.
        let p = change("retry", json!(5)).unwrap();
        assert_eq!(
            p.reqs[0].body,
            Some(
                json!({"flux": "import \"array\"\n\noption task = {name: \"nightly\", every: 1h, offset: 5m, concurrency: 2, retry: 5, foo: 1}\n\narray.from(rows: [{_value: 1}])\n  |> yield()\n"})
            )
        );
        let p = change("concurrency", json!(null)).unwrap();
        let flux = p.reqs[0].body.as_ref().unwrap()["flux"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            flux.contains("{name: \"nightly\", every: 1h, offset: 5m, retry: 3, foo: 1}"),
            "{flux}"
        );
        for (k, v) in [
            ("concurrency", json!(0)),
            ("concurrency", json!(101)),
            ("concurrency", json!("2; x")),
            ("retry", json!(11)),
            ("retry", json!(1.5)),
        ] {
            assert!(
                err(change(k, v.clone())).contains("número entero"),
                "{k} {v}"
            );
        }

        // An expression stays as written, with the other options, until a
        // number is typed.
        let t = task(
            "option task = {name: \"nightly\", every: 1h, offset: 5m, concurrency: n}\n\nn = 2\n",
        );
        let f = form_of(Some(&t), Some(&labels()), None, "dbine");
        assert_eq!(f.values["concurrency"], Value::Null);
        assert_eq!(f.values["other_options"], "concurrency: n");
        let mut x = as_sent(&f);
        assert!(edit_plan(&t, &labels(), &x).unwrap().reqs.is_empty());
        x.values.insert("concurrency".into(), json!(4));
        let p = edit_plan(&t, &labels(), &x).unwrap();
        let flux = p.reqs[0].body.as_ref().unwrap()["flux"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            flux.starts_with(
                "option task = {name: \"nightly\", every: 1h, offset: 5m, concurrency: 4}\n"
            ),
            "{flux}"
        );

        // A new task with both.
        let v = values(json!({
            "name": "c", "mode": "cron", "cron": "0 3 * * *", "concurrency": 3, "retry": "2",
            "flux": "from(bucket: \"b\")"
        }));
        let p = create_plan("o", &[], &v).unwrap();
        assert_eq!(
            p.reqs[0].body.as_ref().unwrap()["flux"],
            "option task = {name: \"c\", cron: \"0 3 * * *\", concurrency: 3, retry: 2}\n\nfrom(bucket: \"b\")\n"
        );
        // Without other options, no field for them.
        let f = form_of(Some(&task(FLUX)), Some(&labels()), None, "dbine");
        assert_eq!(f.values["concurrency"], 1);
        assert!(f
            .sections
            .iter()
            .flat_map(|s| &s.fields)
            .all(|x| x.key != "other_options"));
    }

    #[test]
    fn tasks_dbine_cant_change() {
        // A check's task.
        let mut t = task(FLUX);
        t.kind = Some("threshold".into());
        let f = form_of(Some(&t), Some(&labels()), None, "dbine");
        assert!(f.read_only && f.notes[0].contains("check"));
        assert!(err(edit_plan(&t, &labels(), &as_sent(&f))).contains("check"));
        // It goes with its check: not deleted alone.
        assert!(t.owned());
        // A Cloud script's task: read-only, deleted as any other.
        let mut t = task(FLUX);
        t.flux = String::new();
        assert!(form_of(Some(&t), None, None, "o").read_only);
        assert!(!t.owned());
        // Options DBine doesn't read: the name and schedule stay as the
        // Flux says, and the Flux goes whole.
        let odd = "option task = {name: n, every: 1h}\nfrom(bucket: \"b\")";
        let t = task(odd);
        let f = form_of(Some(&t), Some(&labels()), None, "dbine");
        assert!(field(&f, "name").read_only && field(&f, "every").read_only);
        assert_eq!(f.values["flux"], odd);
        assert!(f.notes.iter().any(|n| n.contains("option task")));
        let mut x = as_sent(&f);
        x.values.insert("name".into(), json!("ignored"));
        x.values.insert(
            "flux".into(),
            json!("option task = {name: n, every: 2h}\nfrom(bucket: \"c\")"),
        );
        let p = edit_plan(&t, &labels(), &x).unwrap();
        assert_eq!(
            p.reqs[0].body,
            Some(json!({"flux": "option task = {name: n, every: 2h}\nfrom(bucket: \"c\")"}))
        );
    }

    #[test]
    fn deleting_and_clearing() {
        let t = task(FLUX);
        let s = drop_plan(&t).script();
        assert!(
            s.contains("// DELETE /api/v2/tasks/0123456789abcdef\n"),
            "{s}"
        );
        assert!(s.contains("_tasks"), "{s}");
        let r = clear_request(&t);
        assert_eq!(
            r.url("http://h", None).unwrap().as_str(),
            "http://h/api/v2/delete?orgID=aaaabbbbccccdddd&bucket=_tasks"
        );
        assert_eq!(
            r.body.unwrap()["predicate"],
            "_measurement=\"runs\" AND taskID=\"0123456789abcdef\""
        );
    }
}

// ---------------------------------------------------------------- 1.x

mod cqs {
    use super::*;
    use crate::scheduler::cq::*;

    const SHOWN: &str = "CREATE CONTINUOUS QUERY \"dbine cq\" ON dbine_db RESAMPLE EVERY 30m FOR 2h BEGIN SELECT mean(v) AS m INTO dbine_db.autogen.\"cpu 1h\" FROM dbine_db.autogen.cpu WHERE host = 'a' GROUP BY time(1h, 5m), * fill(none) END";

    fn dbs() -> Vec<String> {
        vec!["_internal".into(), "dbine_db".into(), "other".into()]
    }

    #[test]
    fn definitions_are_read() {
        let c = parse("dbine_db", "dbine cq", SHOWN);
        assert_eq!(c.every.as_deref(), Some("30m"));
        assert_eq!(c.for_.as_deref(), Some("2h"));
        assert_eq!(
            c.select,
            "SELECT mean(v) AS m INTO dbine_db.autogen.\"cpu 1h\" FROM dbine_db.autogen.cpu WHERE host = 'a' GROUP BY time(1h, 5m), * fill(none)"
        );
        // Names that read like the header's words.
        let q = "CREATE CONTINUOUS QUERY \"a BEGIN b\" ON \"x END\" BEGIN SELECT count(v) INTO o FROM c GROUP BY time(1m) END";
        let c = parse("x END", "a BEGIN b", q);
        assert_eq!((c.every.clone(), c.for_.clone()), (None, None));
        assert_eq!(c.select, "SELECT count(v) INTO o FROM c GROUP BY time(1m)");
        let q = "CREATE CONTINUOUS QUERY c ON d RESAMPLE FOR 2h BEGIN SELECT count(v) INTO o FROM c GROUP BY time(1h) END";
        assert_eq!(parse("d", "c", q).for_.as_deref(), Some("2h"));
    }

    #[test]
    fn the_form_of_a_continuous_query() {
        let c = parse("dbine_db", "dbine cq", SHOWN);
        let f = form_of(Some(&c), &dbs());
        assert_eq!(f.values["database"], "dbine_db");
        assert_eq!(f.values["resample_every"], "30m");
        assert_eq!(f.values["group_by"], "1h");
        assert_eq!(
            f.choices["database"],
            [
                ("dbine_db".to_string(), "dbine_db".to_string()),
                ("other".into(), "other".into())
            ]
        );
        assert!(f.notes[0].contains("ALTER"));
        // Sent back as it came: nothing to do.
        let w = wanted(&as_sent(&f), &dbs()).unwrap();
        let p = edit_plan(&c, &w);
        assert!(p.statements.is_empty());
        assert!(p.script().contains("no hay cambios"));
        let f = form_of(None, &dbs());
        assert_eq!(f.values["name"], "");
        assert!(f.notes.is_empty());
    }

    #[test]
    fn create_change_and_drop() {
        let v = values(json!({
            "database": "dbine_db", "name": "my \"cq\"", "resample_every": "", "resample_for": "2h",
            "query": "SELECT mean(v) INTO o FROM c GROUP BY time(1h) -- a note\n;"
        }));
        let w = wanted(&v, &dbs()).unwrap();
        let p = create_plan(&w);
        assert_eq!(
            p.statements,
            ["CREATE CONTINUOUS QUERY \"my \\\"cq\\\"\" ON \"dbine_db\" RESAMPLE FOR 2h\nBEGIN\nSELECT mean(v) INTO o FROM c GROUP BY time(1h) -- a note\nEND"]
        );
        assert_eq!(p.id, "\"dbine_db\".\"my \\\"cq\\\"\"");
        assert!(p
            .script()
            .starts_with("-- Crea la consulta continua «my \"cq\"»"));
        assert!(p.script().ends_with("END;\n"));

        // Same name: dropped, created, the old one ready to go back.
        let old = parse("dbine_db", "dbine cq", SHOWN);
        let mut v2 = as_sent(&form_of(Some(&old), &dbs()));
        v2.values.insert("resample_every".into(), json!("1h"));
        let p = edit_plan(&old, &wanted(&v2, &dbs()).unwrap());
        assert_eq!(p.statements.len(), 2);
        assert_eq!(
            p.statements[0],
            "DROP CONTINUOUS QUERY \"dbine cq\" ON \"dbine_db\""
        );
        assert!(p.statements[1].starts_with(
            "CREATE CONTINUOUS QUERY \"dbine cq\" ON \"dbine_db\" RESAMPLE EVERY 1h FOR 2h\nBEGIN\n"
        ));
        assert_eq!(p.restore.as_deref(), Some(SHOWN));
        let s = p.script();
        assert!(
            s.starts_with("-- InfluxDB 1.x no tiene ALTER CONTINUOUS QUERY"),
            "{s}"
        );
        // Another name: the new one first, nothing to put back.
        v2.values.insert("name".into(), json!("dbine cq2"));
        v2.values.insert("database".into(), json!("other"));
        let p = edit_plan(&old, &wanted(&v2, &dbs()).unwrap());
        assert!(p.statements[0].starts_with("CREATE CONTINUOUS QUERY \"dbine cq2\" ON \"other\""));
        assert_eq!(
            p.statements[1],
            "DROP CONTINUOUS QUERY \"dbine cq\" ON \"dbine_db\""
        );
        assert!(p.restore.is_none());
        assert_eq!(p.id, "\"other\".\"dbine cq2\"");

        let d = drop_plan(&old).script();
        assert_eq!(
            d,
            "-- Borra la consulta continua «dbine cq» de la base de datos «dbine_db».\nDROP CONTINUOUS QUERY \"dbine cq\" ON \"dbine_db\";\n"
        );
        let mut tricky = old.clone();
        tricky.db = "d'b".into();
        tricky.name = "c\\q".into();
        assert_eq!(
            clear_statement(&tricky),
            "DELETE FROM \"cq_query\" WHERE \"db\" = 'd\\'b' AND \"cq\" = 'c\\\\q'"
        );
    }

    #[test]
    fn values_are_checked() {
        let base = json!({
            "database": "dbine_db", "name": "n",
            "query": "SELECT count(v) INTO o FROM c WHERE t = 'a;b' GROUP BY time(1m)"
        });
        assert!(
            wanted(&values(base.clone()), &dbs()).is_ok(),
            "a ; in a string"
        );
        let with = |k: &str, v: Value| {
            let mut b = base.clone();
            b[k] = v;
            values(b)
        };
        let cases = [
            (with("database", json!("")), "Falta la base"),
            (with("database", json!("nope")), "No existe la base"),
            (with("name", json!("")), "necesita un nombre"),
            (with("name", json!("a\rb")), "saltos de línea"),
            (with("resample_every", json!("1mo")), "RESAMPLE EVERY"),
            (with("resample_for", json!("2 h")), "RESAMPLE FOR"),
            (with("query", json!("")), "necesita su SELECT"),
            (
                with(
                    "query",
                    json!("SELECT count(v) INTO o FROM c GROUP BY time(1m); DROP DATABASE x"),
                ),
                "un solo SELECT",
            ),
            (
                with("query", json!("DROP DATABASE x")),
                "empezar con SELECT",
            ),
            (
                with("query", json!("SELECT count(v) FROM c GROUP BY time(1m)")),
                "INTO",
            ),
            (
                with("query", json!("SELECT count(v) INTO o FROM c")),
                "GROUP BY time",
            ),
            (
                with(
                    "query",
                    json!("SELECT count(v) INTO o FROM c GROUP BY \"time(\""),
                ),
                "GROUP BY time",
            ),
        ];
        for (v, msg) in cases {
            let m = err(wanted(&v, &dbs()));
            assert!(m.contains(msg), "{msg}: {m}");
        }
    }
}

// ---------------------------------------------------------------- 3.x

mod triggers {
    use super::*;
    use crate::scheduler::trigger::*;

    fn dbs() -> Vec<String> {
        vec!["db".into(), "db2".into()]
    }

    fn old() -> Trigger {
        Trigger {
            db: "db".into(),
            name: "tick".into(),
            plugin: "p.py".into(),
            spec: "every:5s".into(),
            disabled: false,
            error_behavior: "retry".into(),
            args: vec![("k".into(), "v".into()), ("a".into(), "1,2".into())],
        }
    }

    #[test]
    fn the_form_of_a_trigger() {
        let f = form_of(
            Some(&old()),
            &dbs(),
            Some((
                "/plugins/p.py".into(),
                Some(10),
                Some("2026-11-12T00:00:00Z".into()),
            )),
        );
        assert_eq!(f.values["fires"], "every");
        assert_eq!(f.values["every"], "5s");
        assert_eq!(f.values["cron"], "");
        assert_eq!(f.values["enabled"], true);
        assert_eq!(f.values["error_behavior"], "retry");
        assert_eq!(f.values["run_async"], Value::Null);
        assert_eq!(f.values["plugin_path"], "/plugins/p.py");
        assert!(field(&f, "plugin_path").read_only);
        let list = &f.sections[3].lists[0];
        assert_eq!(list.rows.len(), 2);
        assert_eq!(list.rows[1]["value"], "1,2");
        assert_eq!(f.notes.len(), 2);
        // Sent back as it came: nothing to do.
        let w = wanted(&as_sent(&f), &dbs()).unwrap();
        let p = edit_plan(&old(), &w, &[]);
        assert!(p.reqs.is_empty(), "{p:?}");
        assert!(p.script().contains("no hay cambios"));

        let mut t = old();
        t.spec = "request:hook".into();
        let f = form_of(Some(&t), &dbs(), None);
        assert_eq!(
            (f.values["fires"].clone(), f.values["path"].clone()),
            (json!("request"), json!("hook"))
        );
        let f = form_of(None, &dbs(), None);
        assert_eq!(f.values["every"], "1m");
        assert_eq!(f.values["run_async"], false);
        assert!(f.notes.is_empty());
    }

    #[test]
    fn a_new_trigger() {
        let v = with_list(
            values(json!({
                "database": "db", "name": "n", "plugin": "dir/p.py", "fires": "cron",
                "cron": "0 0 3 * * *", "enabled": false, "error_behavior": "disable",
                "run_async": true, "plugin_code": "def process_scheduled_call(a, b, c=None):\n    pass\n"
            })),
            "arguments",
            json!([{"key": "x", "value": "1"}, {"key": "", "value": ""}]),
        );
        let w = wanted(&v, &dbs()).unwrap();
        let p = create_plan(&w, &["other".to_string()]);
        assert_eq!(p.id, "\"db\".\"n\"");
        assert_eq!(p.reqs.len(), 2);
        assert_eq!(
            (p.reqs[0].method, p.reqs[0].path.as_str()),
            ("POST", "/api/v3/plugins/files")
        );
        assert_eq!(p.reqs[0].body.as_ref().unwrap()["plugin_name"], "dir/p.py");
        assert_eq!(
            p.reqs[1].body,
            Some(json!({
                "db": "db", "plugin_filename": "dir/p.py", "trigger_name": "n",
                "trigger_specification": "cron:0 0 3 * * *",
                "trigger_arguments": {"x": "1"},
                "trigger_settings": {"run_async": true, "error_behavior": "disable"},
                "disabled": true
            }))
        );
        let s = p.script();
        assert!(s.contains("también lo usan: other"), "{s}");
        assert!(s.lines().all(|l| l.starts_with("--")), "{s}");

        for (fires, extra, spec) in [
            ("every", json!({"every": "1h 30m"}), "every:1h 30m"),
            ("table", json!({"table": "cpu"}), "table:cpu"),
            ("all_tables", json!({}), "all_tables"),
            ("request", json!({"path": "/hook/x"}), "request:hook/x"),
        ] {
            let mut b = json!({"database": "db2", "name": "n", "plugin": "gh:examples/x.py", "fires": fires});
            for (k, v) in extra.as_object().unwrap() {
                b[k] = v.clone();
            }
            let w = wanted(&values(b), &dbs()).unwrap();
            assert_eq!(w.trigger.spec, spec);
            assert!(!w.trigger.disabled);
            assert_eq!(w.trigger.error_behavior, "log");
            let p = create_plan(&w, &[]);
            assert_eq!(p.reqs.len(), 1);
            assert_eq!(
                p.reqs[0].body.as_ref().unwrap()["trigger_arguments"],
                Value::Null
            );
        }
    }

    #[test]
    fn values_are_checked() {
        let base = json!({"database": "db", "name": "n", "plugin": "p.py", "fires": "every", "every": "10s"});
        let with = |k: &str, v: Value| {
            let mut b = base.clone();
            b[k] = v;
            values(b)
        };
        let cases = [
            (with("database", json!("nope")), "No existe la base"),
            (with("name", json!("")), "necesita un nombre"),
            (with("name", json!("a\nb")), "saltos de línea"),
            (with("plugin", json!("")), "Falta el plugin"),
            (with("plugin", json!("../etc/x.py")), "no es un plugin"),
            (with("plugin", json!("/etc/x.py")), "no es un plugin"),
            (with("plugin", json!("gh:../x.py")), "no es un plugin"),
            (with("every", json!("10")), "no es una duración"),
            (with("fires", json!("cron")), "Falta la expresión cron"),
            (with("fires", json!("nope")), "no es una forma"),
            (with("error_behavior", json!("explode")), "comportamiento"),
        ];
        for (v, msg) in cases {
            let m = err(wanted(&v, &dbs()));
            assert!(m.contains(msg), "{msg}: {m}");
        }
        let mut cron = base.clone();
        cron["fires"] = json!("cron");
        cron["cron"] = json!("0 3 * * *");
        assert!(err(wanted(&values(cron), &dbs())).contains("seis campos"));
        let mut req = base.clone();
        req["fires"] = json!("request");
        req["path"] = json!("a/../b");
        assert!(err(wanted(&values(req.clone()), &dbs())).contains("no es una ruta"));
        req["path"] = json!("a?b=1");
        assert!(err(wanted(&values(req), &dbs())).contains("no es una ruta"));
        let mut gh = base.clone();
        gh["plugin"] = json!("gh:x/y.py");
        gh["plugin_code"] = json!("print(1)");
        assert!(err(wanted(&values(gh), &dbs())).contains("gh:"));
        let dup = with_list(
            values(base.clone()),
            "arguments",
            json!([{"key": "a", "value": "1"}, {"key": "a", "value": "2"}]),
        );
        assert!(err(wanted(&dup, &dbs())).contains("dos veces"));
        let nokey = with_list(
            values(base),
            "arguments",
            json!([{"key": "", "value": "2"}]),
        );
        assert!(err(wanted(&nokey, &dbs())).contains("su clave"));
    }

    #[test]
    fn changes_in_place_or_again() {
        let f = form_of(Some(&old()), &dbs(), None);
        let base = as_sent(&f);
        let plan = |k: &str, v: Value| {
            let mut x = base.clone();
            x.values.insert(k.into(), v);
            edit_plan(&old(), &wanted(&x, &dbs()).unwrap(), &[])
        };
        // Enabled: in place.
        let p = plan("enabled", json!(false));
        assert_eq!(p.reqs.len(), 1);
        assert_eq!(
            p.reqs[0].url("http://h", None).unwrap().as_str(),
            "http://h/api/v3/configure/processing_engine_trigger/disable?db=db&trigger_name=tick"
        );
        assert!(p.undo.is_none());
        // The plugin's code: in place.
        let p = plan("plugin_code", json!("print(2)"));
        assert_eq!(p.reqs[0].method, "PUT");
        assert_eq!(
            p.reqs[0].url("http://h", None).unwrap().as_str(),
            "http://h/api/v3/plugins/files?db=db"
        );
        assert_eq!(
            p.reqs[0].body,
            Some(json!({"plugin_name": "tick", "content": "print(2)"}))
        );
        // The schedule: deleted and created again, the old one ready to go
        // back.
        let p = plan("every", json!("1m"));
        assert_eq!(p.reqs.len(), 2);
        assert_eq!(p.reqs[0].method, "DELETE");
        assert_eq!(
            p.reqs[0].body,
            Some(json!({"db": "db", "trigger_name": "tick", "force": true}))
        );
        assert_eq!(
            p.reqs[1].body.as_ref().unwrap()["trigger_specification"],
            "every:1m"
        );
        assert_eq!(p.undo_after, Some(0));
        let undo = p.undo.as_ref().unwrap().body.as_ref().unwrap();
        assert_eq!(undo["trigger_specification"], "every:5s");
        assert_eq!(undo["trigger_arguments"], json!({"a": "1,2", "k": "v"}));
        let s = p.script();
        assert!(s.contains("se borra y se vuelve a crear"), "{s}");
        assert!(s.contains("modo síncrono"), "{s}");
        // Asynchronous runs asked for: created again with them.
        let p = plan("run_async", json!(true));
        assert_eq!(
            p.reqs[1].body.as_ref().unwrap()["trigger_settings"]["run_async"],
            true
        );
        // Code and a new schedule: the file first.
        let mut x = base.clone();
        x.values.insert("plugin_code".into(), json!("print(3)"));
        x.values.insert("fires".into(), json!("all_tables"));
        let p = edit_plan(&old(), &wanted(&x, &dbs()).unwrap(), &[]);
        assert_eq!(p.reqs[0].path, "/api/v3/plugins/files");
        assert_eq!(p.reqs[0].method, "POST");
        assert_eq!(p.undo_after, Some(1));
        // Another name: the new one first, then the old one goes.
        let p = plan("name", json!("tock"));
        assert_eq!(p.reqs[0].method, "POST");
        assert_eq!(p.reqs[1].method, "DELETE");
        assert!(p.undo.is_none());
        assert_eq!(p.id, "\"db\".\"tock\"");

        let d = drop_plan(&old()).script();
        assert!(
            d.contains("-- DELETE /api/v3/configure/processing_engine_trigger"),
            "{d}"
        );
        assert!(d.contains("\"force\": true"), "{d}");
    }

    #[test]
    fn the_log() {
        let cols: Vec<String> = [
            "event_time",
            "database_name",
            "trigger_name",
            "log_level",
            "log_text",
            "error_details",
        ]
        .map(String::from)
        .to_vec();
        let rows = vec![vec![
            json!("2026-11-12 21:58:50.03"),
            json!("db"),
            json!("tick"),
            json!("ERROR"),
            json!("boom"),
            json!("Traceback"),
        ]];
        let r = log_rows(&cols, &rows, None);
        assert_eq!(
            r[0],
            [
                json!("2026-11-12 21:58:50.03"),
                json!("db"),
                json!("tick"),
                json!("ERROR"),
                json!("boom — Traceback")
            ]
        );
        let cols: Vec<String> = ["event_time", "trigger_name", "log_level", "log_text"]
            .map(String::from)
            .to_vec();
        let r = log_rows(
            &cols,
            &[vec![json!("t"), json!("x"), json!("INFO"), json!("ok")]],
            Some("db2"),
        );
        assert_eq!(r[0][1], "db2");
        assert_eq!(r[0][4], "ok");
    }
}

#[test]
fn every_api_offers_the_scheduler() {
    for d in crate::drivers() {
        assert!(d.capabilities().scheduler, "{}", d.info().id);
    }
}

#[test]
fn tasks_run_by_a_check_or_a_script_are_listed_read_only() {
    let basic = json!({"id": "0a1b2c3d4e5f6a7b", "name": "t", "status": "active", "flux": "option task = {name: \"t\", every: 1h}\nfrom(bucket: \"b\")"});
    let it = task::item_of(&basic).unwrap();
    assert_eq!(
        (it.can_edit, it.can_delete, it.enabled),
        (None, None, Some(true))
    );
    // A list that leaves the Flux out doesn't make it a script's.
    let mut no_flux = basic.clone();
    no_flux.as_object_mut().unwrap().remove("flux");
    assert_eq!(task::item_of(&no_flux).unwrap().can_edit, None);
    // A check's or a rule's task goes with its check: neither changed nor
    // deleted here. A Cloud script's is deleted as any other.
    for (managed, can_delete) in [
        (
            json!({"id": "0a1b2c3d4e5f6a7b", "name": "c", "status": "inactive", "type": "threshold", "flux": "x"}),
            Some(false),
        ),
        (
            json!({"id": "0a1b2c3d4e5f6a7b", "name": "s", "status": "active", "scriptID": "0a1b", "flux": ""}),
            None,
        ),
        (
            json!({"id": "0a1b2c3d4e5f6a7b", "name": "s", "status": "active", "flux": ""}),
            None,
        ),
    ] {
        let it = task::item_of(&managed).unwrap();
        assert_eq!(
            (it.can_edit, it.can_delete),
            (Some(false), can_delete),
            "{managed}"
        );
    }
    assert!(task::item_of(&json!({"id": "x' or 1=1", "name": "t"})).is_none());
}

#[test]
fn clearing_task_history_takes_write_on_the_tasks_bucket() {
    let list = |perms: &[Value]| {
        let mut ps = perms.to_vec();
        // A token that reads authorizations sees its own among them.
        ps.push(json!({"action": "read", "resource": {"type": "authorizations"}}));
        Ok(json!({"authorizations": [{"status": "active", "permissions": ps}]}))
    };
    let tasks_rw = [
        json!({"action": "read", "resource": {"type": "tasks", "orgID": "0a1"}}),
        json!({"action": "write", "resource": {"type": "tasks", "orgID": "0a1"}}),
    ];
    // Tasks only, no write on buckets: the delete API would refuse it.
    assert!(!task::clears_history(list(&tasks_rw), "0a1"));
    // Write on another bucket only.
    let other = json!({"action": "write", "resource": {"type": "buckets", "id": "b1", "name": "test", "orgID": "0a1"}});
    assert!(!task::clears_history(list(&[other]), "0a1"));
    // Write on every bucket of the org, or on `_tasks`.
    let all = json!({"action": "write", "resource": {"type": "buckets", "orgID": "0a1"}});
    assert!(task::clears_history(list(&[all]), "0a1"));
    let tasks_bucket = json!({"action": "write", "resource": {"type": "buckets", "id": "b2", "name": "_tasks", "orgID": "0a1"}});
    assert!(task::clears_history(list(&[tasks_bucket]), "0a1"));
    // Can't tell (the token doesn't read authorizations): left on.
    assert!(task::clears_history(
        Ok(json!({"authorizations": []})),
        "0a1"
    ));
    assert!(task::clears_history(
        Err(dbine_driver::Error::AuthFailed("forbidden".into())),
        "0a1"
    ));
}
