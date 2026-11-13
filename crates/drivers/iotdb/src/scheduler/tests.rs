use super::pipe::{self, Change, Clause, Val};
use super::*;
use crate::jobs::{Cq, Pipe};
use dbine_driver::ResultColumn;
use serde_json::json;

fn values(v: Value) -> FormValues {
    let mut out = FormValues::default();
    for (k, x) in v.as_object().unwrap() {
        if let Value::Array(rows) = x {
            if rows.first().is_some_and(Value::is_object) || k.ends_with("_attrs") || k == "secrets"
            {
                out.lists.insert(
                    k.clone(),
                    rows.iter()
                        .map(|r| {
                            r.as_object()
                                .unwrap()
                                .iter()
                                .map(|(a, b)| (a.clone(), b.clone()))
                                .collect()
                        })
                        .collect(),
                );
                continue;
            }
        }
        out.values.insert(k.clone(), x.clone());
    }
    out
}

const V132: Ctx = Ctx {
    version: (1, 3, 2),
    cq: true,
    pipe: true,
};
const V205: Ctx = Ctx {
    version: (2, 0, 5),
    cq: true,
    pipe: true,
};
const V12: Ctx = Ctx {
    version: (1, 2, 2),
    cq: true,
    pipe: true,
};

/// The script's "⚠" lines, as the app lists them above it.
fn warnings(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|l| l.strip_prefix("-- ⚠ "))
        .map(str::to_string)
        .collect()
}

fn real(p: &Plan) -> Vec<String> {
    p.stmts.iter().map(|s| s.real.clone().unwrap()).collect()
}

// ------------------------------------------------------------------ general

#[test]
fn kinds_ids_and_versions() {
    assert_eq!(version_of("1.3.2"), (1, 3, 2));
    assert_eq!(version_of("2.0.5"), (2, 0, 5));
    assert_eq!(version_of("TimechoDB 1.3.4.2"), (1, 3, 4));
    assert_eq!(version_of(""), (1, 3, 0));
    assert!(Kind::parse("job").is_ok() && Kind::parse("pipe_plugin").is_ok());
    assert!(Kind::parse("schedule").is_err());
    assert_eq!(target(Kind::Job, "cq:a b").unwrap(), Target::Cq("a b"));
    assert_eq!(target(Kind::Job, "pipe:p").unwrap(), Target::Pipe("p"));
    assert_eq!(target(Kind::Pipe, "pipe:p").unwrap(), Target::Pipe("p"));
    assert!(target(Kind::Pipe, "cq:x").is_err());
    assert!(target(Kind::Job, "cq:").is_err());
    assert!(target(Kind::Job, "p").is_err());
    assert_eq!(
        target(Kind::Plugin, "plugin:X").unwrap(),
        Target::Plugin("X")
    );
    assert!(V205.alters(Clause::Source) && !V132.alters(Clause::Source));
    assert!(V132.alters(Clause::Sink) && !V12.alters(Clause::Processor));
}

#[test]
fn the_kinds_follow_the_privileges() {
    let i = info_of(&V205);
    let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "pipe", "pipe_plugin"]);
    assert!(i
        .kinds
        .iter()
        .all(|k| k.can_create && k.can_edit && k.can_delete));
    assert!(!i.logs && i.notes.is_empty());
    let only_pipes = Ctx { cq: false, ..V205 };
    let i = info_of(&only_pipes);
    assert!(
        !i.kinds[0].can_create && i.kinds[0].can_edit,
        "pipes in the jobs list stay editable"
    );
    assert!(i.notes[0].contains("USE_CQ"));
    let neither = Ctx {
        cq: false,
        pipe: false,
        ..V205
    };
    let i = info_of(&neither);
    assert!(i
        .kinds
        .iter()
        .all(|k| !k.can_create && !k.can_edit && !k.can_delete));
    assert_eq!(i.notes.len(), 2);
    assert!(check_allowed(&only_pipes, Kind::Job, None).is_err());
    assert!(check_allowed(&only_pipes, Kind::Job, Some("pipe:p")).is_ok());
    assert!(check_allowed(&neither, Kind::Plugin, Some("plugin:X")).is_err());
}

#[test]
fn literals_escape_quotes_and_refuse_what_iotdb_cant_read() {
    assert_eq!(lit("it's", "x").unwrap(), "'it''s'");
    assert_eq!(lit(r"C:\dir\file", "x").unwrap(), r"'C:\dir\file'");
    assert_eq!(lit(r"a\\", "x").unwrap(), r"'a\\'");
    assert_eq!(lit(r"a\\'b", "x").unwrap(), r"'a\\''b'");
    assert!(lit(r"a\", "x").is_err());
    assert!(lit(r"a\'; DROP DATABASE root.x", "x").is_err());
    assert!(lit("a\0b", "x").is_err());
    assert_eq!(ident("a`b"), "`a``b`");
}

#[test]
fn durations_and_instants() {
    assert_eq!(duration("30s", "x").unwrap(), "30s");
    assert_eq!(duration(" 1H30M ", "x").unwrap(), "1h30m");
    assert_eq!(duration("1mo2w500ms10us", "x").unwrap(), "1mo2w500ms10us");
    for bad in ["", "s", "10", "10x", "1h ; DROP", "-1s", "1.5h"] {
        assert!(duration(bad, "x").is_err(), "{bad}");
    }
    assert_eq!(
        instant("2024-01-01T00:00:00", "x").unwrap(),
        "2024-01-01T00:00:00"
    );
    assert_eq!(
        instant("2024-01-01 08:00:00", "x").unwrap(),
        "2024-01-01T08:00:00"
    );
    assert!(instant("2024/01/01T08:00:00.123+08:00", "x").is_ok());
    assert!(instant("1700000000000", "x").is_ok());
    assert!(instant("-5", "x").is_ok());
    assert!(instant("2024-01-01", "x").is_ok());
    for bad in [
        "now()",
        "2024-1-1",
        "2024-01-01T25",
        "2024-01-01T00:00:00 END",
        "x",
    ] {
        assert!(instant(bad, "x").is_err(), "{bad}");
    }
}

#[test]
fn a_plan_without_statements_says_so() {
    let p = Plan::new("a\nb", "cq:a".into());
    assert_eq!(p.script(), "-- «a b»: no hay cambios que guardar.\n");
}

// ------------------------------------------------------------- continuous queries

const Q1: &str = "CREATE CQ dbine_test_sched_cq1 RESAMPLE EVERY 30s BOUNDARY 2024-01-01T00:00:00 RANGE 1m, 10s TIMEOUT POLICY DISCARD BEGIN SELECT count(s1) INTO root.dbine_test_sched.agg(c) FROM root.dbine_test_sched.d1 GROUP BY(30s) END";
const Q2: &str = "CREATE CONTINUOUS QUERY `my cq``s` BEGIN\n  SELECT CASE WHEN s1 > 1 THEN 1 ELSE 0 END INTO root.a.b(c) FROM root.a.d GROUP BY(1m)\nEND";

fn cq(id: &str, q: &str) -> Cq {
    Cq {
        id: id.into(),
        query: Some(q.into()),
        state: "ACTIVE".into(),
    }
}

#[test]
fn continuous_queries_read_back() {
    let d = cq::parse(Q1).unwrap();
    assert_eq!(d.name, "dbine_test_sched_cq1");
    assert_eq!(d.every.as_deref(), Some("30s"));
    assert_eq!(d.boundary.as_deref(), Some("2024-01-01T00:00:00"));
    assert_eq!(d.range_start.as_deref(), Some("1m"));
    assert_eq!(d.range_end.as_deref(), Some("10s"));
    assert!(d.discard);
    assert_eq!(
        d.query,
        "SELECT count(s1) INTO root.dbine_test_sched.agg(c) FROM root.dbine_test_sched.d1 GROUP BY(30s)"
    );
    let d = cq::parse(Q2).unwrap();
    assert_eq!(d.name, "my cq`s");
    assert!(d.every.is_none() && d.range_start.is_none() && !d.discard);
    assert!(
        d.query.contains("ELSE 0 END INTO"),
        "the last END closes it: {}",
        d.query
    );
    let d = cq::parse(
        "create cq c resample range 2m begin select 1 into root.a.b(c) from root.a.d end;",
    )
    .unwrap();
    assert_eq!(d.range_start.as_deref(), Some("2m"));
    assert!(cq::parse("SELECT 1").is_none());
    assert!(cq::parse("CREATE CQ x BEGIN SELECT 1 END trailing").is_none());
    // What it writes reads back the same.
    let d = cq::parse(Q1).unwrap();
    assert_eq!(cq::parse(&cq::create(&d)).unwrap(), d);
}

#[test]
fn continuous_query_scripts() {
    let v = values(json!({
        "name": "dbine_test_sched_cq1", "every": "30S", "boundary": "2024-01-01 00:00:00",
        "range_start": "1m", "range_end": "10s", "timeout_policy": "discard",
        "query": "SELECT count(s1) INTO root.sg.agg(c) FROM root.sg.d1 GROUP BY(30s);"
    }));
    let p = cq::plan(None, &v).unwrap();
    assert_eq!(p.id, "cq:dbine_test_sched_cq1");
    assert_eq!(
        p.script(),
        "-- Consulta continua «dbine_test_sched_cq1» de IoTDB\nCREATE CONTINUOUS QUERY `dbine_test_sched_cq1`\nRESAMPLE EVERY 30s BOUNDARY 2024-01-01T00:00:00 RANGE 1m, 10s\nTIMEOUT POLICY DISCARD\nBEGIN\nSELECT count(s1) INTO root.sg.agg(c) FROM root.sg.d1 GROUP BY(30s)\nEND;\n"
    );
    // Only the query: no RESAMPLE, no TIMEOUT POLICY.
    let v = values(
        json!({"name": "c", "timeout_policy": "blocked", "query": "select 1 into root.a.b(c) from root.a.d"}),
    );
    let p = cq::plan(None, &v).unwrap();
    assert_eq!(
        real(&p),
        ["CREATE CONTINUOUS QUERY `c`\nBEGIN\nselect 1 into root.a.b(c) from root.a.d\nEND"]
    );
}

#[test]
fn continuous_query_checks() {
    let base = json!({"name": "c", "query": "SELECT 1 INTO root.a.b(c) FROM root.a.d"});
    let with = |k: &str, x: Value| {
        let mut b = base.clone();
        b[k] = x;
        values(b)
    };
    let err = |v: FormValues| cq::plan(None, &v).unwrap_err().to_string();
    assert!(err(with("name", json!(""))).contains("necesita un nombre"));
    assert!(err(with("name", json!("a\nb"))).contains("no es un nombre"));
    assert!(err(with("every", json!("5 minutes"))).contains("duración"));
    assert!(err(with("boundary", json!("ayer"))).contains("fecha"));
    assert!(err(with("range_end", json!("1m"))).contains("desde hace"));
    assert!(err(with("timeout_policy", json!("wait"))).contains("política"));
    assert!(err(with("query", json!(""))).contains("necesita su consulta"));
    assert!(err(with("query", json!("DELETE FROM root.a.d"))).contains("SELECT … INTO"));
    assert!(err(with("query", json!("SELECT 1 FROM root.a.d"))).contains("SELECT … INTO"));
    assert!(err(with(
        "query",
        json!("SELECT 1 INTO root.a.b(c) FROM root.a.d; DROP DATABASE root.a")
    ))
    .contains("sola sentencia"));
    assert!(err(with(
        "query",
        json!("SELECT 1 INTO root.a.b(c) FROM root.a.d WHERE x = 'a")
    ))
    .contains("sin cerrar"));
    assert!(err(with(
        "query",
        json!("SELECT (1 INTO root.a.b(c) FROM root.a.d")
    ))
    .contains("sin cerrar"));
    // A ; inside quotes is text.
    assert!(cq::plan(
        None,
        &with(
            "query",
            json!("SELECT 1 INTO root.a.b(c) FROM root.a.d WHERE s = 'a;b' AND t = 'it\\'s'")
        )
    )
    .is_ok());
}

#[test]
fn a_changed_continuous_query_is_dropped_and_created_again() {
    let current = cq("dbine_test_sched_cq1", Q1);
    let form = cq::form(&V205, Some(&current));
    assert!(!form.read_only);
    assert_eq!(form.values["every"], json!("30s"));
    assert_eq!(form.values["timeout_policy"], json!("discard"));
    assert_eq!(form.values["state"], json!("ACTIVE"));
    assert!(form.notes.iter().any(|n| n.contains("DROP CQ")));
    // Unchanged (case and the 0 end aside): nothing to do.
    let mut v = FormValues {
        values: form.values.clone(),
        lists: Default::default(),
    };
    v.values.insert("every".into(), json!("30S"));
    let p = cq::plan(Some(&current), &v).unwrap();
    assert!(p.stmts.is_empty(), "{}", p.script());
    assert!(p.script().contains("no hay cambios"));
    // A new schedule: drop, create, and the old statement to put back.
    v.values.insert("every".into(), json!("1m"));
    let p = cq::plan(Some(&current), &v).unwrap();
    let r = real(&p);
    assert_eq!(r[0], "DROP CQ `dbine_test_sched_cq1`");
    assert!(r[1].contains("RESAMPLE EVERY 1m BOUNDARY"), "{}", r[1]);
    let rc = p.recreate.as_ref().unwrap();
    assert_eq!((rc.drop, rc.create), (0, 1));
    assert_eq!(rc.restore[0].real.as_deref().unwrap(), Q1);
    assert!(p.script().starts_with(
        "-- Consulta continua «dbine_test_sched_cq1» de IoTDB\n-- ⚠ IoTDB no tiene ALTER"
    ));
    // Renamed: the new id.
    v.values
        .insert("name".into(), json!("dbine_test_sched_cq2"));
    let p = cq::plan(Some(&current), &v).unwrap();
    assert_eq!(p.id, "cq:dbine_test_sched_cq2");
    assert!(real(&p)[1].starts_with("CREATE CONTINUOUS QUERY `dbine_test_sched_cq2`"));
    // Deleting.
    let p = cq::drop_plan(&current);
    assert_eq!(p.script(), "-- Borra la consulta continua «dbine_test_sched_cq1» de IoTDB\nDROP CQ `dbine_test_sched_cq1`;\n");
    // A statement DBine can't read: shown read-only.
    let odd = cq("x", "CREATE CQ x WEIRD");
    let form = cq::form(&V205, Some(&odd));
    assert!(form.read_only && form.notes[0].contains("no pudo leer"));
    assert!(cq::plan(Some(&odd), &v).is_err());
    // A user without USE_CQ sees it read-only.
    let form = cq::form(&Ctx { cq: false, ..V205 }, Some(&current));
    assert!(form.read_only && form.notes[0].contains("USE_CQ"));
    // A new one: defaults and the note.
    let form = cq::form(&V205, None);
    assert!(form.notes[0].contains("Pipes"));
    assert!(form
        .sections
        .iter()
        .all(|s| s.fields.iter().all(|f| f.key != "state")));
}

// ------------------------------------------------------------------- pipes

fn table(cols: &[&str], rows: Vec<Vec<Value>>) -> Table {
    Table {
        columns: cols
            .iter()
            .map(|n| ResultColumn {
                name: (*n).into(),
                type_name: String::new(),
            })
            .collect(),
        rows,
    }
}

fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A 1.x pipe: the server shows the password.
fn pipe_v1() -> Pipe {
    Pipe {
        name: "dbine_test_sched_p1".into(),
        state: "RUNNING".into(),
        source: kv(&[("source", "iotdb-source"), ("source.pattern", "root.a")]),
        processor: kv(&[("processor", "do-nothing-processor")]),
        sink: kv(&[
            ("sink", "iotdb-thrift-sink"),
            ("sink.node-urls", "h:6667"),
            ("sink.password", "s3cret"),
            ("sink.user", "root"),
        ]),
        ..Pipe::default()
    }
}

/// A 2.x pipe: passwords hidden, the creator's added to the source.
fn pipe_v2() -> Pipe {
    Pipe {
        name: "dbine_test_sched_p2".into(),
        state: "STOPPED".into(),
        source: kv(&[
            ("__system.sql-dialect", "tree"),
            ("source", "iotdb-source"),
            ("source.password", "******"),
            ("source.path", "root.a.**"),
            ("source.username", "root"),
        ]),
        processor: Vec::new(),
        sink: kv(&[
            ("sink", "do-nothing-sink"),
            ("sink.password", "******"),
            ("sink.username", "root"),
        ]),
        ..Pipe::default()
    }
}

/// What the designer sends back for a form as it came.
fn back(form: &ObjectForm) -> FormValues {
    FormValues {
        values: form.values.clone(),
        lists: form
            .sections
            .iter()
            .flat_map(|s| s.lists.iter())
            .map(|l| (l.key.clone(), l.rows.clone()))
            .collect(),
    }
}

#[test]
fn a_new_pipe() {
    let v = values(json!({
        "name": "dbine_test_sched_p",
        "enabled": false,
        "source": "IOTDB-SOURCE",
        "source_attrs": [{"key": "source.path", "value": "root.a.**"}, {"key": "", "value": ""}],
        "sink": "iotdb-thrift-sink",
        "sink_attrs": [{"key": "sink.node-urls", "value": "h1:6667, h2:6667"}, {"key": "sink.note", "value": "it's"}],
        "secrets": [{"clause": "sink", "key": "sink.password", "value": "pa'ss"}]
    }));
    let p = pipe::plan(&V205, None, &v).unwrap();
    assert_eq!(p.id, "pipe:dbine_test_sched_p");
    assert_eq!(
        p.script(),
        "-- Pipe «dbine_test_sched_p» de IoTDB\n-- Arranca al crearse: se detiene enseguida (STOP PIPE).\nCREATE PIPE `dbine_test_sched_p`\nWITH SOURCE ('source'='iotdb-source', 'source.path'='root.a.**')\nWITH SINK ('sink'='iotdb-thrift-sink', 'sink.node-urls'='h1:6667, h2:6667', 'sink.note'='it''s', 'sink.password'='********');\nSTOP PIPE `dbine_test_sched_p`;\n"
    );
    let r = real(&p);
    assert!(r[0].ends_with("'sink.password'='pa''ss')"), "{}", r[0]);
    assert!(!p.script().contains("pa''ss"));
    // A password typed in the plain attributes is masked in the script too.
    let v = values(json!({"name": "p", "sink": "do-nothing-sink",
        "sink_attrs": [{"key": "sink.password", "value": "plain"}]}));
    let p = pipe::plan(&V205, None, &v).unwrap();
    assert!(!p.script().contains("plain") && real(&p)[0].contains("'plain'"));
    // A credential row without a value can't be written.
    let v = values(json!({"name": "p", "sink": "do-nothing-sink",
        "secrets": [{"clause": "sink", "key": "sink.password", "value": null}]}));
    let p = pipe::plan(&V205, None, &v).unwrap();
    assert!(p.script().contains("'sink.password'='********'"));
    assert!(p.stmts[0]
        .real
        .as_ref()
        .unwrap_err()
        .contains("Falta el valor"));
    // Said in the script, not as a consequence to accept: the save
    // refuses it.
    assert!(
        p.script()
            .contains("\n-- Falta el valor de la credencial «sink.password».\n"),
        "{}",
        p.script()
    );
    assert!(warnings(&p.script()).is_empty(), "{}", p.script());
}

#[test]
fn pipe_checks() {
    let err = |v: Value| pipe::plan(&V205, None, &values(v)).unwrap_err().to_string();
    assert!(err(json!({"name": "p"})).contains("destino"));
    assert!(err(json!({"name": "p", "sink": "a b"})).contains("plugin"));
    assert!(
        err(json!({"name": "p", "sink": "s", "sink_attrs": [{"key": "sink", "value": "x"}]}))
            .contains("su campo")
    );
    assert!(err(
        json!({"name": "p", "sink": "s", "source_attrs": [{"key": "extractor", "value": "x"}]})
    )
    .contains("su campo"));
    assert!(err(
        json!({"name": "p", "sink": "s", "sink_attrs": [{"key": "sink.x'", "value": "x"}]})
    )
    .contains("no es un atributo"));
    assert!(err(
        json!({"name": "p", "sink": "s", "sink_attrs": [{"key": "__system.x", "value": "x"}]})
    )
    .contains("no es un atributo"));
    assert!(
        err(json!({"name": "p", "sink": "s", "sink_attrs": [{"key": "", "value": "x"}]}))
            .contains("no tiene nombre")
    );
    assert!(err(
        json!({"name": "p", "sink": "s", "sink_attrs": [{"key": "sink.a", "value": "1"}],
        "secrets": [{"clause": "sink", "key": "SINK.A", "value": "2"}]})
    )
    .contains("dos veces"));
    assert!(err(
        json!({"name": "p", "sink": "s", "secrets": [{"clause": "x", "key": "k", "value": "2"}]})
    )
    .contains("qué parte"));
    assert!(err(
        json!({"name": "p", "sink": "s", "sink_attrs": [{"key": "sink.a", "value": "x\\"}]})
    )
    .contains("barra invertida"));
}

#[test]
fn a_pipe_form_hides_its_credentials() {
    let plugins = table(
        &["PluginName", "PluginType", "ClassName", "PluginJar"],
        vec![
            vec![json!("IOTDB-SOURCE"), json!("Builtin"), json!("org.apache.iotdb.commons.pipe.plugin.builtin.extractor.iotdb.IoTDBExtractor"), json!("")],
            vec![json!("DO-NOTHING-PROCESSOR"), json!("Builtin"), json!("org.apache.iotdb.commons.pipe.agent.plugin.builtin.processor.donothing.DoNothingProcessor"), json!("")],
            vec![json!("DO-NOTHING-SINK"), json!("Builtin"), json!("org.apache.iotdb.commons.pipe.agent.plugin.builtin.sink.donothing.DoNothingSink"), json!("")],
            vec![json!("MY-PLUGIN"), json!("External"), json!("org.example.Mine"), json!("mine.jar")],
        ],
    );
    let p = pipe_v2();
    let f = pipe::form_of(&V205, Some(&p), Some(&plugins));
    assert_eq!(f.values["sink"], json!("do-nothing-sink"));
    assert_eq!(f.values["enabled"], json!(false));
    assert_eq!(
        f.choices["pipe_plugin:source"],
        vec![
            ("iotdb-source".into(), "iotdb-source".into()),
            ("my-plugin".into(), "my-plugin".into())
        ]
    );
    assert_eq!(f.choices["pipe_plugin:sink"].len(), 2);
    let lists: BTreeMap<&str, &Vec<BTreeMap<String, Value>>> = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l| (l.key.as_str(), &l.rows))
        .collect();
    assert_eq!(
        lists["source_attrs"].len(),
        2,
        "path and username, not the system key nor the password"
    );
    assert_eq!(lists["secrets"].len(), 2);
    assert!(lists["secrets"].iter().all(|r| r["value"].is_null()));
    let all = serde_json::to_string(&f).unwrap();
    assert!(!all.contains("******") && !all.contains("__system"));
    assert!(f.notes.iter().any(|n| n.contains("conservar")));
    // 1.x shows the password: the form still doesn't.
    let f = pipe::form_of(&V132, Some(&pipe_v1()), None);
    assert!(!serde_json::to_string(&f).unwrap().contains("s3cret"));
    assert!(f.notes.iter().any(|n| n.contains("del origen")));
    // IoTDB's own pipes are read-only.
    let sys = Pipe {
        name: "__consensus.x".into(),
        ..pipe_v1()
    };
    let f = pipe::form_of(&V205, Some(&sys), None);
    assert!(f.read_only);
    assert!(pipe::plan(&V205, Some(&sys), &back(&f)).is_err());
    assert!(pipe::drop_plan(&sys).is_err());
}

/// The list a form has under `key`.
fn list<'a>(f: &'a ObjectForm, key: &str) -> &'a dbine_driver::FormList {
    f.sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .find(|l| l.key == key)
        .unwrap()
}

#[test]
fn the_processor_attributes_show_once_a_processor_is_picked() {
    let plugins = table(
        &["PluginName", "PluginType", "ClassName", "PluginJar"],
        vec![
            vec![json!("DO-NOTHING-PROCESSOR"), json!("Builtin"), json!("org.apache.iotdb.commons.pipe.agent.plugin.builtin.processor.donothing.DoNothingProcessor"), json!("")],
            vec![json!("DO-NOTHING-SINK"), json!("Builtin"), json!("org.apache.iotdb.commons.pipe.agent.plugin.builtin.sink.donothing.DoNothingSink"), json!("")],
            vec![json!("MY-PLUGIN"), json!("External"), json!("org.example.Mine"), json!("mine.jar")],
        ],
    );
    let p = pipe_v2();
    let f = pipe::form_of(&V205, Some(&p), Some(&plugins));
    assert_eq!(
        list(&f, "processor_attrs").when,
        Some((
            "processor".to_string(),
            vec!["do-nothing-processor".to_string(), "my-plugin".to_string()]
        ))
    );
    // The other lists always show.
    assert!(list(&f, "source_attrs").when.is_none() && list(&f, "sink_attrs").when.is_none());
    // Hidden, it isn't sent: nothing changes.
    let mut v = back(&f);
    v.lists.remove("processor_attrs");
    let plan = pipe::plan(&V205, Some(&p), &v).unwrap();
    assert!(plan.stmts.is_empty(), "{}", plan.script());
    // Without the plugins listed it can't say which: always shown.
    assert!(
        list(&pipe::form_of(&V205, Some(&p), None), "processor_attrs")
            .when
            .is_none()
    );
    // Attributes without a processor named stay in view.
    let stray = Pipe {
        processor: kv(&[("processor.down-sampling.interval-seconds", "60")]),
        ..pipe_v2()
    };
    let f = pipe::form_of(&V205, Some(&stray), Some(&plugins));
    assert!(list(&f, "processor_attrs").when.is_none());
    assert_eq!(list(&f, "processor_attrs").rows.len(), 1);
}

#[test]
fn items_the_user_cant_change_say_so() {
    let cq = Cq {
        id: "dbine_test_sched_cq".into(),
        state: "ACTIVE".into(),
        ..Cq::default()
    };
    let it = cq_item(&cq, true);
    assert_eq!((it.can_edit, it.can_delete), (None, None));
    let it = cq_item(&cq, false);
    assert_eq!((it.can_edit, it.can_delete), (Some(false), Some(false)));
    let it = pipe_item(&pipe_v2(), true);
    assert_eq!((it.can_edit, it.can_delete, it.actions), (None, None, None));
    assert_eq!(pipe_item(&pipe_v2(), false).can_edit, Some(false));
    let sys = Pipe {
        name: "__consensus.x".into(),
        ..pipe_v1()
    };
    let it = pipe_item(&sys, true);
    assert!(it.system);
    assert_eq!((it.can_edit, it.can_delete), (Some(false), Some(false)));
}

#[test]
fn an_unchanged_pipe_needs_nothing() {
    for (ctx, p) in [(V205, pipe_v2()), (V132, pipe_v1())] {
        let f = pipe::form_of(&ctx, Some(&p), None);
        let plan = pipe::plan(&ctx, Some(&p), &back(&f)).unwrap();
        assert!(plan.stmts.is_empty(), "{}", plan.script());
    }
}

#[test]
fn pipe_changes_go_through_alter_pipe() {
    let p = pipe_v2();
    let f = pipe::form_of(&V205, Some(&p), None);
    let mut v = back(&f);
    // A new attribute and a changed one in the sink, a typed password in
    // the source, and started: MODIFY both, then START.
    v.lists.get_mut("sink_attrs").unwrap().push(BTreeMap::from([
        ("key".to_string(), json!("sink.batch.enable")),
        ("value".to_string(), json!("false")),
    ]));
    for r in v.lists.get_mut("secrets").unwrap() {
        if r["key"] == json!("source.password") {
            r.insert("value".into(), json!("newpw"));
        }
    }
    v.values.insert("enabled".into(), json!(true));
    let plan = pipe::plan(&V205, Some(&p), &v).unwrap();
    assert_eq!(
        plan.stmts.iter().map(|s| s.shown.as_str()).collect::<Vec<_>>(),
        [
            "ALTER PIPE `dbine_test_sched_p2`\nMODIFY SOURCE ('source.password'='********')\nMODIFY SINK ('sink.batch.enable'='false')",
            "START PIPE `dbine_test_sched_p2`"
        ]
    );
    assert!(real(&plan)[0].contains("'source.password'='newpw'"));
    assert!(plan.recreate.is_none());
    // An attribute taken away: REPLACE, which needs the hidden password.
    let mut v = back(&f);
    v.lists.get_mut("sink_attrs").unwrap().clear();
    let plan = pipe::plan(&V205, Some(&p), &v).unwrap();
    assert!(
        plan.stmts[0]
            .shown
            .contains("REPLACE SINK ('sink'='do-nothing-sink', 'sink.password'='********')"),
        "{}",
        plan.script()
    );
    let e = plan.stmts[0].real.as_ref().unwrap_err();
    assert!(e.contains("no devuelve") && e.contains("SINK"), "{e}");
    // Typed again, it goes.
    for r in v.lists.get_mut("secrets").unwrap() {
        r.insert("value".into(), json!("pw"));
    }
    let plan = pipe::plan(&V205, Some(&p), &v).unwrap();
    assert!(
        real(&plan)[0].contains("REPLACE SINK ('sink'='do-nothing-sink', 'sink.password'='pw')")
    );
    // The credential taken away too: REPLACE without it.
    let mut v = back(&f);
    v.lists.get_mut("sink_attrs").unwrap().clear();
    v.lists
        .get_mut("secrets")
        .unwrap()
        .retain(|r| r["clause"] != json!("sink"));
    let plan = pipe::plan(&V205, Some(&p), &v).unwrap();
    assert_eq!(
        real(&plan)[0],
        "ALTER PIPE `dbine_test_sched_p2`\nREPLACE SINK ('sink'='do-nothing-sink')"
    );
    // On 2.0 the source changes in place.
    let mut v = back(&f);
    for r in v.lists.get_mut("source_attrs").unwrap() {
        if r["key"] == json!("source.path") {
            r.insert("value".into(), json!("root.b.**"));
        }
    }
    let plan = pipe::plan(&V205, Some(&p), &v).unwrap();
    assert_eq!(
        real(&plan),
        ["ALTER PIPE `dbine_test_sched_p2`\nMODIFY SOURCE ('source.path'='root.b.**')"]
    );
    // Stopping only.
    let mut v = back(&pipe::form_of(&V132, Some(&pipe_v1()), None));
    v.values.insert("enabled".into(), json!(false));
    assert_eq!(
        real(&pipe::plan(&V132, Some(&pipe_v1()), &v).unwrap()),
        ["STOP PIPE `dbine_test_sched_p1`"]
    );
}

#[test]
fn what_alter_pipe_cant_change_recreates_the_pipe() {
    let p = pipe_v1();
    let f = pipe::form_of(&V132, Some(&p), None);
    // 1.3 can't alter the source: drop and create, the server's password
    // sent back (1.x shows it), the old pipe to put back.
    let mut v = back(&f);
    for r in v.lists.get_mut("source_attrs").unwrap() {
        r.insert("value".into(), json!("root.b"));
    }
    let plan = pipe::plan(&V132, Some(&p), &v).unwrap();
    let r = real(&plan);
    assert_eq!(r[0], "DROP PIPE `dbine_test_sched_p1`");
    assert!(
        r[1].contains("'source.pattern'='root.b'") && r[1].contains("'sink.password'='s3cret'"),
        "{}",
        r[1]
    );
    assert!(!plan.script().contains("s3cret"));
    assert!(plan.script().contains("ALTER PIPE (llegó en la 2.0)"));
    let rc = plan.recreate.as_ref().unwrap();
    assert_eq!((rc.drop, rc.create), (0, 1));
    let restore = rc.restore[0].real.as_ref().unwrap();
    assert!(
        restore.contains("'source.pattern'='root.a'")
            && restore.contains("'sink.password'='s3cret'")
    );
    assert!(!rc.restore[0].shown.contains("s3cret"));
    // Before 1.3 every change does.
    let mut v = back(&pipe::form_of(&V12, Some(&p), None));
    v.lists.get_mut("sink_attrs").unwrap().push(BTreeMap::from([
        ("key".to_string(), json!("sink.batch.enable")),
        ("value".to_string(), json!("false")),
    ]));
    let plan = pipe::plan(&V12, Some(&p), &v).unwrap();
    assert!(plan.script().contains("no tiene ALTER PIPE"));
    assert_eq!(plan.stmts.len(), 2);
    // A rename: create the new one, then drop the old one.
    let mut v = back(&f);
    v.values
        .insert("name".into(), json!("dbine_test_sched_p1b"));
    let plan = pipe::plan(&V132, Some(&p), &v).unwrap();
    let r = real(&plan);
    assert!(r[0].starts_with("CREATE PIPE `dbine_test_sched_p1b`"));
    assert_eq!(r[1], "DROP PIPE `dbine_test_sched_p1`");
    assert_eq!(plan.id, "pipe:dbine_test_sched_p1b");
    assert!(plan.recreate.is_none());
    // On 2.x a rename needs the hidden passwords typed again.
    let p2 = pipe_v2();
    let mut v = back(&pipe::form_of(&V205, Some(&p2), None));
    v.values.insert("name".into(), json!("x"));
    let plan = pipe::plan(&V205, Some(&p2), &v).unwrap();
    assert!(plan.stmts[0].real.is_err());
}

#[test]
fn pipe_diffs() {
    let cur = kv(&[("sink", "IOTDB-THRIFT-SINK"), ("sink.a", "1")]);
    let same = vec![
        ("sink".to_string(), Val::Plain("iotdb-thrift-sink".into())),
        ("sink.a".to_string(), Val::Plain("1".into())),
    ];
    assert_eq!(pipe::diff(Clause::Sink, &cur, &same), Change::Same);
    let removed = vec![("sink".to_string(), Val::Plain("iotdb-thrift-sink".into()))];
    assert_eq!(pipe::diff(Clause::Sink, &cur, &removed), Change::Replace);
    let kept = vec![
        ("sink".to_string(), Val::Plain("iotdb-thrift-sink".into())),
        ("sink.a".to_string(), Val::Secret(None)),
    ];
    assert_eq!(pipe::diff(Clause::Sink, &cur, &kept), Change::Same);
    // `connector` of an old pipe stays its key.
    let old = Pipe {
        sink: kv(&[
            ("connector", "iotdb-thrift-connector"),
            ("connector.ip", "h"),
        ]),
        ..pipe_v1()
    };
    let f = pipe::form_of(&V132, Some(&old), None);
    assert_eq!(f.values["sink"], json!("iotdb-thrift-connector"));
    let plan = pipe::plan(&V132, Some(&old), &back(&f)).unwrap();
    assert!(plan.stmts.is_empty(), "{}", plan.script());
}

// ------------------------------------------------------------------ plugins

#[test]
fn pipe_plugins() {
    let t = table(
        &["PluginName", "PluginType", "ClassName", "PluginJar"],
        vec![
            vec![json!("DO-NOTHING-SINK"), json!("Builtin"), json!("org.apache.iotdb.commons.pipe.plugin.builtin.connector.donothing.DoNothingConnector"), json!("")],
            vec![json!("DBINE_TEST_SCHED_PLUGIN"), json!("External"), json!("org.example.Mine"), json!("mine-ecf8.jar")],
        ],
    );
    let all = plugin::plugins(&t);
    assert_eq!(all[0].role(), Some(Clause::Sink));
    assert_eq!(all[1].role(), None);
    let items: Vec<SchedulerItem> = all.iter().map(plugin::item).collect();
    assert!(items[0].system && !items[1].system);
    assert_eq!(
        (items[0].can_edit, items[0].can_delete),
        (Some(false), Some(false))
    );
    assert_eq!((items[1].can_edit, items[1].can_delete), (None, None));
    assert_eq!(items[1].id, "plugin:DBINE_TEST_SCHED_PLUGIN");
    // Built-in: read-only.
    let f = plugin::form(&V205, Some(&all[0]));
    assert!(f.read_only);
    assert!(plugin::drop_plan(&all[0]).is_err());
    // New.
    let v = values(
        json!({"name": "dbine_test_sched_plugin", "class_name": "org.example.Mine", "uri": "file:///opt/it's.jar"}),
    );
    let p = plugin::plan(None, &v).unwrap();
    assert_eq!(
        real(&p),
        ["CREATE PIPEPLUGIN `dbine_test_sched_plugin` AS 'org.example.Mine' USING URI 'file:///opt/it''s.jar'"]
    );
    assert_eq!(p.id, "plugin:DBINE_TEST_SCHED_PLUGIN");
    let err = |v: Value| plugin::plan(None, &values(v)).unwrap_err().to_string();
    assert!(err(json!({"name": "p", "class_name": "org.example.Mine"})).contains("URI"));
    assert!(
        err(json!({"name": "p", "class_name": "org.example.Mine'; x", "uri": "u"}))
            .contains("clase Java")
    );
    assert!(
        err(json!({"name": "p q", "class_name": "a.B", "uri": "u"})).contains("nombre de plugin")
    );
    // Existing: same values and no URI: nothing; a new class without the
    // URI: refused; with it: drop and load again.
    let f = plugin::form(&V205, Some(&all[1]));
    assert!(!f.read_only && f.notes[0].contains("URI"));
    let mut v = back(&f);
    let p = plugin::plan(Some(&all[1]), &v).unwrap();
    assert!(p.stmts.is_empty());
    v.values
        .insert("class_name".into(), json!("org.example.Other"));
    assert!(plugin::plan(Some(&all[1]), &v).is_err());
    v.values.insert("uri".into(), json!("file:///x.jar"));
    let p = plugin::plan(Some(&all[1]), &v).unwrap();
    assert_eq!(real(&p)[0], "DROP PIPEPLUGIN `DBINE_TEST_SCHED_PLUGIN`");
    assert_eq!(
        warnings(&p.script()),
        [
            "IoTDB no cambia un plugin cargado: se borra y se vuelve a cargar desde la URI.",
            "Si la carga falla, el plugin queda borrado: IoTDB no guarda la URI para volver a cargarlo.",
        ]
    );
    assert!(real(&p)[1]
        .starts_with("CREATE PIPEPLUGIN `DBINE_TEST_SCHED_PLUGIN` AS 'org.example.Other'"));
    // The script of a loaded plugin (the designer's "Script"): the JAR's
    // name in place of the URI, never saved.
    let mut v_script = back(&f);
    v_script.values.remove("uri");
    let p = plugin::plan(None, &v_script).unwrap();
    assert!(p.script().contains("USING URI 'mine-ecf8.jar'") && p.script().contains("reemplazalo"));
    assert!(p.stmts[0].real.is_err());
    // Renamed: load the new one first.
    v.values.insert("name".into(), json!("other"));
    let p = plugin::plan(Some(&all[1]), &v).unwrap();
    assert!(real(&p)[0].starts_with("CREATE PIPEPLUGIN `other`"));
    assert_eq!(p.id, "plugin:OTHER");
    assert_eq!(
        warnings(&p.script()),
        [
            "IoTDB no cambia un plugin cargado: se borra y se vuelve a cargar desde la URI.",
            "Los pipes que lo usan por su nombre anterior dejan de encontrarlo.",
        ]
    );
    // Created or deleted: nothing to accept.
    assert!(warnings(
        &plugin::plan(
            None,
            &values(json!({"name": "n", "class_name": "a.B", "uri": "u"}))
        )
        .unwrap()
        .script()
    )
    .is_empty());
    assert!(warnings(&plugin::drop_plan(&all[1]).unwrap().script()).is_empty());
    assert_eq!(
        plugin::drop_plan(&all[1]).unwrap().script(),
        "-- Borra el plugin de pipe «DBINE_TEST_SCHED_PLUGIN» de IoTDB\nDROP PIPEPLUGIN `DBINE_TEST_SCHED_PLUGIN`;\n"
    );
}

#[test]
fn credentials_never_reach_a_message() {
    let e = scrub(
        Error::Query("line 1:88 near 'pa''ss' and pa'ss".into()),
        &["pa'ss".to_string()],
    );
    assert_eq!(e.to_string(), "line 1:88 near '********' and ********");
    let e = scrub(Error::Query("no secret".into()), &["x1".to_string()]);
    assert_eq!(e.to_string(), "no secret");
    let v = values(json!({"name": "p", "sink": "do-nothing-sink",
        "secrets": [{"clause": "sink", "key": "sink.password", "value": "s3cr'et"}]}));
    let p = pipe::plan(&V205, None, &v).unwrap();
    assert_eq!(p.stmts[0].secrets, ["s3cr'et"]);
}

#[test]
fn a_continuous_query_made_again_says_so() {
    let current = cq("dbine_test_sched_cq1", Q1);
    let f = cq::form(&V205, Some(&current));
    let mut v = FormValues {
        values: f.values.clone(),
        lists: Default::default(),
    };
    assert!(warnings(&cq::plan(Some(&current), &v).unwrap().script()).is_empty());
    v.values.insert("every".into(), json!("1m"));
    let p = cq::plan(Some(&current), &v).unwrap();
    assert_eq!(
        warnings(&p.script()),
        ["IoTDB no tiene ALTER para las consultas continuas: se borra y se vuelve a crear."]
    );
    assert!(warnings(&cq::plan(None, &v).unwrap().script()).is_empty());
    assert!(warnings(&cq::drop_plan(&current).script()).is_empty());
}

#[test]
fn a_pipe_change_warns_only_where_it_starts_over() {
    // In place: ALTER PIPE keeps its progress.
    let p2 = pipe_v2();
    let f = pipe::form_of(&V205, Some(&p2), None);
    let mut v = back(&f);
    for r in v.lists.get_mut("source_attrs").unwrap() {
        if r["key"] == json!("source.path") {
            r.insert("value".into(), json!("root.b.**"));
        }
    }
    let plan = pipe::plan(&V205, Some(&p2), &v).unwrap();
    assert!(!plan.stmts.is_empty() && warnings(&plan.script()).is_empty());
    // REPLACE without the credential IoTDB 2.x hides: it has to be typed
    // again, and the script says so.
    let mut v = back(&f);
    v.lists.get_mut("sink_attrs").unwrap().clear();
    let plan = pipe::plan(&V205, Some(&p2), &v).unwrap();
    let ws = warnings(&plan.script());
    assert_eq!(ws.len(), 1, "{}", plan.script());
    assert!(ws[0].contains("IoTDB no devuelve el valor de esta credencial"));
    for r in v.lists.get_mut("secrets").unwrap() {
        r.insert("value".into(), json!("pw"));
    }
    assert!(warnings(&pipe::plan(&V205, Some(&p2), &v).unwrap().script()).is_empty());
    // A new credential left without a value: a plain comment (the save
    // refuses it), nothing to accept.
    let mut v = back(&f);
    let row = json!({"clause": "sink", "key": "sink.token", "value": null});
    v.lists
        .entry("secrets".into())
        .or_default()
        .push(serde_json::from_value(row).unwrap());
    let plan = pipe::plan(&V205, Some(&p2), &v).unwrap();
    assert!(
        plan.script()
            .contains("\n-- Falta el valor de la credencial «sink.token».\n"),
        "{}",
        plan.script()
    );
    assert!(warnings(&plan.script()).is_empty(), "{}", plan.script());

    // 1.3 can't alter the source: dropped and created, it starts over.
    let p1 = pipe_v1();
    let f = pipe::form_of(&V132, Some(&p1), None);
    let mut v = back(&f);
    for r in v.lists.get_mut("source_attrs").unwrap() {
        r.insert("value".into(), json!("root.b"));
    }
    let ws = warnings(&pipe::plan(&V132, Some(&p1), &v).unwrap().script());
    assert_eq!(ws.len(), 2, "{ws:?}");
    assert!(ws[0].ends_with("se borra y se vuelve a crear."));
    assert!(ws[1].starts_with("Volver a crear un pipe lo hace empezar de cero"));
    // A rename of a running pipe: created again, it starts over.
    let mut v = back(&f);
    v.values
        .insert("name".into(), json!("dbine_test_sched_p1b"));
    let ws = warnings(&pipe::plan(&V132, Some(&p1), &v).unwrap().script());
    assert_eq!(ws.len(), 2, "{ws:?}");
    assert!(ws[0].starts_with("IoTDB no permite renombrar un pipe"));
    // A stopped one kept stopped: the new one runs until it's stopped, and
    // on 2.x its hidden credentials have to be typed again.
    let mut v = back(&pipe::form_of(&V205, Some(&p2), None));
    v.values.insert("name".into(), json!("x"));
    let ws = warnings(&pipe::plan(&V205, Some(&p2), &v).unwrap().script());
    assert!(
        ws.iter()
            .any(|w| w.starts_with("El pipe nuevo arranca al crearse")),
        "{ws:?}"
    );
    assert!(
        ws.iter().any(|w| w.contains("no devuelve el valor")),
        "{ws:?}"
    );
    // Created or deleted: nothing.
    let v = values(json!({"name": "p", "sink": "do-nothing-sink"}));
    assert!(warnings(&pipe::plan(&V205, None, &v).unwrap().script()).is_empty());
    assert!(warnings(&pipe::drop_plan(&p1).unwrap().script()).is_empty());
}

#[test]
fn the_job_kinds_items_say_they_have_no_steps_nor_history() {
    let cq = Cq {
        id: "c".into(),
        state: "ACTIVE".into(),
        ..Cq::default()
    };
    for it in [cq_item(&cq, true), pipe_item(&pipe_v2(), true)] {
        assert_eq!(
            (it.run_from_step, it.clear_history),
            (Some(false), Some(false))
        );
    }
}
