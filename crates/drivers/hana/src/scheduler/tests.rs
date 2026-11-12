//! Unit tests of the scheduler jobs designer: values, scripts, the form
//! (no server: SAP HANA isn't in the test containers).

use super::*;
use serde_json::json;

fn fv(values: Value, params: Value) -> FormValues {
    FormValues {
        values: serde_json::from_value(values).unwrap(),
        lists: BTreeMap::from([(
            "parameters".to_string(),
            serde_json::from_value(params).unwrap(),
        )]),
    }
}

fn base() -> Value {
    json!({
        "schema": "APP",
        "name": "NIGHTLY",
        "procedure": "\"APP\".\"PURGE\"",
        "cron": "* * * * 3 0 0",
        "enabled": true
    })
}

fn proc_params() -> Vec<ProcParam> {
    vec![
        ProcParam {
            name: "DAYS".into(),
            data_type: "INTEGER".into(),
            mode: "IN".into(),
        },
        ProcParam {
            name: "tag".into(),
            data_type: "NVARCHAR".into(),
            mode: "IN".into(),
        },
        ProcParam {
            name: "FLAG".into(),
            data_type: "BOOLEAN".into(),
            mode: "IN".into(),
        },
        ProcParam {
            name: "RESULT".into(),
            data_type: "INTEGER".into(),
            mode: "OUT".into(),
        },
    ]
}

fn current() -> Job {
    Job {
        schema: "APP".into(),
        name: "NIGHTLY".into(),
        owner: Some("ANA".into()),
        proc_schema: Some("APP".into()),
        proc_name: Some("PURGE".into()),
        cron: "* * * * 3 0 0".into(),
        enabled: true,
        valid: Some("TRUE".into()),
        comments: Some("purga".into()),
        from: None,
        until: None,
        created: Some("2026-11-01T10:00:00Z".into()),
        params: vec![("DAYS".into(), "7".into())],
    }
}

/// The plan of `values` against `cur` (with the procedure's parameters).
fn plan_of(cur: Option<&Job>, values: Value, params: Value) -> Result<Plan> {
    let w = wanted(&fv(values, params), true)?;
    let typed = w.typed(&proc_params())?;
    plan(cur, &w, &typed, true)
}

#[test]
fn only_the_job_kind_exists() {
    assert!(check_kind("job").is_ok());
    assert!(check_kind("program").is_err());
}

#[test]
fn crons_are_checked() {
    for ok in [
        "* * * * 3 0 0",
        "2026 1:6 * * */2 0,30 0",
        "* * -1 * 3 0 0",
        "* * * mon,fri 8 0 0",
        "* * * MON:FRI 8 0 0",
        "* * * * 0:23/4 0 0",
        "* * 1.2 * 3 0 0",
    ] {
        assert!(check_cron(ok).is_ok(), "{ok}: {:?}", check_cron(ok));
    }
    assert_eq!(check_cron(" *  * * * 3 0\t0 ").unwrap(), "* * * * 3 0 0");
    for bad in [
        "",
        "* * * * 3 0",
        "* * * * 3 0 0 0",
        "* * * * 3 0 0'",
        "* * * mon;drop 3 0 0",
        "* * * * 3 0 0\n",
        "* * * funday 3 0 0",
        "* mon * * 3 0 0",
        "* * * * 3 ,0 0",
    ] {
        assert!(check_cron(bad).is_err(), "{bad:?}");
    }
    assert!(check_cron("* * *").unwrap_err().contains("tiene 3"));
}

#[test]
fn times_are_the_servers_local_timestamps() {
    assert_eq!(
        timestamp("2026-11-12T10:30").as_deref(),
        Some("2026-11-12 10:30:00")
    );
    assert_eq!(
        timestamp(" 2026-11-12 10:30:15.250 ").as_deref(),
        Some("2026-11-12 10:30:15")
    );
    assert_eq!(
        timestamp("2028-02-29T00:00:00").as_deref(),
        Some("2028-02-29 00:00:00")
    );
    for bad in [
        "",
        "2026-11-12",
        "2026-02-30T00:00",
        "2027-02-29T00:00",
        "2026-13-01T00:00",
        "2026-11-12T24:00",
        "2026-11-12T10:60",
        "2026-11-12T10:30:00Z",
        "2026-11-12T10:30:00+03:00",
        "2026-11-12X10:30",
        "26-11-12T10:30",
        "2026-11-12T10:30:00.",
        "2026-11-12T10:30'; DROP",
    ] {
        assert_eq!(timestamp(bad), None, "{bad:?}");
    }
}

#[test]
fn parameter_values_follow_their_type() {
    assert_eq!(param_literal(" 7 ", Some("INTEGER"), "D").unwrap(), "7");
    assert_eq!(
        param_literal("-1.5e3", Some("decimal"), "D").unwrap(),
        "-1.5e3"
    );
    for bad in ["7; DROP TABLE X", "x", "", "NaN", "inf", "1e999", "0x10"] {
        assert!(param_literal(bad, Some("BIGINT"), "D").is_err(), "{bad:?}");
    }
    assert_eq!(param_literal("true", Some("BOOLEAN"), "F").unwrap(), "TRUE");
    assert_eq!(param_literal("0", Some("BOOLEAN"), "F").unwrap(), "FALSE");
    assert!(param_literal("maybe", Some("BOOLEAN"), "F").is_err());
    // Everything else is a string literal, quotes doubled.
    assert_eq!(
        param_literal("it's'); DROP TABLE X; --", Some("NVARCHAR"), "T").unwrap(),
        "'it''s''); DROP TABLE X; --'"
    );
    assert_eq!(param_literal("7", Some("NVARCHAR"), "T").unwrap(), "'7'");
    assert_eq!(
        param_literal("2026-01-01", Some("DATE"), "T").unwrap(),
        "'2026-01-01'"
    );
    // Unknown type: a number as it is, else text.
    assert_eq!(param_literal("7", None, "T").unwrap(), "7");
    assert_eq!(param_literal("'x'", None, "T").unwrap(), "'''x'''");
    assert_eq!(param_ident("DAYS"), "DAYS");
    assert_eq!(param_ident("days"), "\"days\"");
    assert_eq!(param_ident("A\"B"), "\"A\"\"B\"");
    assert_eq!(unquote_value("'it''s'"), "it's");
    assert_eq!(unquote_value(" 7 "), "7");
}

#[test]
fn values_are_checked() {
    let w = wanted(
        &fv(
            json!({
                "schema": " APP ", "name": "NIGHTLY", "procedure": "\"APP\".\"PURGE\"",
                "cron": "* * * * 3 0 0", "from": "2026-11-12T10:00", "until": "2027-01-01T00:00:00",
                "enabled": false, "comments": "  purga  "
            }),
            json!([{"name": "DAYS", "value": "7"}]),
        ),
        true,
    )
    .unwrap();
    assert_eq!(w.schema, "APP");
    assert_eq!(
        (w.proc_schema.as_str(), w.proc_name.as_str()),
        ("APP", "PURGE")
    );
    assert_eq!(w.from.as_deref(), Some("2026-11-12 10:00:00"));
    assert_eq!(w.until.as_deref(), Some("2027-01-01 00:00:00"));
    assert!(!w.enabled);
    assert_eq!(w.comments.as_deref(), Some("purga"));
    // Without COMMENTS in the catalog the comment is ignored.
    let mut v = base();
    v["comments"] = json!("x");
    assert_eq!(wanted(&fv(v, json!([])), false).unwrap().comments, None);

    let err = |v: Value, p: Value| wanted(&fv(v, p), true).unwrap_err().to_string();
    let with = |k: &str, x: Value| {
        let mut v = base();
        v[k] = x;
        v
    };
    assert!(err(with("schema", json!("")), json!([])).contains("esquema"));
    assert!(err(with("name", json!("a\nb")), json!([])).contains("nombre de job válido"));
    assert!(err(with("name", json!("x".repeat(128))), json!([])).contains("127"));
    assert!(err(with("procedure", json!("")), json!([])).contains("procedimiento"));
    assert!(
        err(with("procedure", json!("APP.PURGE")), json!([])).contains("no es un procedimiento")
    );
    assert!(err(with("cron", json!("")), json!([])).contains("cron"));
    assert!(err(with("cron", json!("* * *")), json!([])).contains("7 campos"));
    assert!(err(with("from", json!("mañana")), json!([])).contains("Desde"));
    let mut v = base();
    v["from"] = json!("2027-01-01T00:00");
    v["until"] = json!("2026-01-01T00:00");
    assert!(err(v, json!([])).contains("posterior"));
    assert!(err(base(), json!([{"name": "", "value": "1"}])).contains("parámetro"));
    assert!(err(
        base(),
        json!([{"name": "A", "value": "1"}, {"name": "A", "value": "2"}])
    )
    .contains("dos veces"));
    assert!(err(with("comments", json!("x".repeat(5001))), json!([])).contains("5000"));
}

#[test]
fn parameters_are_the_procedures() {
    let w = wanted(
        &fv(
            base(),
            json!([
                {"name": "days", "value": "7"},
                {"name": "tag", "value": "a'b"},
                {"name": "FLAG", "value": "true"}
            ]),
        ),
        true,
    )
    .unwrap();
    // `days` matches `DAYS` (HANA folds unquoted names); `tag` stays quoted.
    assert_eq!(
        w.typed(&proc_params()).unwrap(),
        [
            ("DAYS".to_string(), "7".to_string()),
            ("tag".to_string(), "'a''b'".to_string()),
            ("FLAG".to_string(), "TRUE".to_string())
        ]
    );
    let e = |p: Value| {
        wanted(&fv(base(), p), true)
            .unwrap()
            .typed(&proc_params())
            .unwrap_err()
            .to_string()
    };
    assert!(e(json!([{"name": "NOPE", "value": "1"}])).contains("no tiene el parámetro"));
    assert!(e(json!([{"name": "RESULT", "value": "1"}])).contains("OUT"));
    assert!(e(json!([{"name": "DAYS", "value": "x"}])).contains("no es un número"));
    // Parameters not visible: written as typed.
    let w = wanted(&fv(base(), json!([{"name": "X", "value": "abc"}])), true).unwrap();
    assert_eq!(
        w.typed(&[]).unwrap(),
        [("X".to_string(), "'abc'".to_string())]
    );
}

#[test]
fn a_new_job_is_its_create_and_its_comment() {
    let mut v = base();
    v["from"] = json!("2026-11-12T10:00");
    v["enabled"] = json!(false);
    v["comments"] = json!("it's -- ok; really");
    let p = plan_of(
        None,
        v,
        json!([{"name": "DAYS", "value": "7"}, {"name": "tag", "value": "x"}]),
    )
    .unwrap();
    assert_eq!(p.id, "\"APP\".\"NIGHTLY\"");
    assert_eq!(
        p.statements,
        [
            "CREATE SCHEDULER JOB \"APP\".\"NIGHTLY\"\n  CRON '* * * * 3 0 0'\n  FROM '2026-11-12 10:00:00'\n  DISABLE\n  PROCEDURE \"APP\".\"PURGE\"\n  PARAMETERS DAYS = 7, \"tag\" = 'x'",
            "COMMENT ON SCHEDULER JOB \"APP\".\"NIGHTLY\" IS 'it''s -- ok; really'"
        ]
    );
    assert!(p.script.starts_with("-- Scheduler job \"APP\".\"NIGHTLY\" de SAP HANA\n-- DBine guarda estas sentencias en una sola transacción"));
    assert_eq!(p.drops, None);
    // No comment, no parameters: one statement.
    let p = plan_of(None, base(), json!([])).unwrap();
    assert_eq!(
        p.statements,
        ["CREATE SCHEDULER JOB \"APP\".\"NIGHTLY\"\n  CRON '* * * * 3 0 0'\n  ENABLE\n  PROCEDURE \"APP\".\"PURGE\""]
    );
    assert!(!p.script.contains("una sola transacción"));
    // Names with quotes are quoted whole.
    let mut v = base();
    v["name"] = json!("my \"job\"");
    let p = plan_of(None, v, json!([])).unwrap();
    assert!(p.statements[0].starts_with("CREATE SCHEDULER JOB \"APP\".\"my \"\"job\"\"\""));
}

#[test]
fn an_edit_alters_only_what_changed() {
    let cur = current();
    let same = || {
        let mut v = base();
        v["comments"] = json!("purga");
        v
    };
    let p = plan_of(Some(&cur), same(), json!([{"name": "DAYS", "value": "7"}])).unwrap();
    assert!(p.statements.is_empty());
    assert!(p.script.contains("no hay cambios"));
    assert_eq!(p.id, "\"APP\".\"NIGHTLY\"");

    let mut v = same();
    v["cron"] = json!("* * * * 4 30 0");
    v["until"] = json!("2027-01-01T00:00");
    let p = plan_of(Some(&cur), v, json!([{"name": "DAYS", "value": "7"}])).unwrap();
    assert_eq!(
        p.statements,
        ["ALTER SCHEDULER JOB \"APP\".\"NIGHTLY\" CRON '* * * * 4 30 0' UNTIL '2027-01-01 00:00:00'"]
    );
    assert!(p.script.contains("cambia en el lugar"));

    let mut v = same();
    v["enabled"] = json!(false);
    v["comments"] = json!("");
    let p = plan_of(Some(&cur), v, json!([{"name": "DAYS", "value": "8"}])).unwrap();
    assert_eq!(
        p.statements,
        [
            "ALTER SCHEDULER JOB \"APP\".\"NIGHTLY\" DISABLE",
            "ALTER SCHEDULER JOB \"APP\".\"NIGHTLY\" PARAMETERS DAYS = 8",
            "COMMENT ON SCHEDULER JOB \"APP\".\"NIGHTLY\" IS NULL"
        ]
    );
    assert!(p.script.contains("una sola transacción"));
    assert_eq!(p.drops, None, "in place: nothing dropped");

    // A quoted value the catalog gives is the same value.
    let mut quoted = current();
    quoted.params = vec![("tag".into(), unquote_value("'x'"))];
    let p = plan_of(
        Some(&quoted),
        same(),
        json!([{"name": "tag", "value": "x"}]),
    )
    .unwrap();
    assert!(p.statements.is_empty(), "{:?}", p.statements);
}

#[test]
fn what_alter_cannot_change_creates_the_job_again() {
    let cur = current();
    // A new name: create the new one, then drop the old.
    let mut v = base();
    v["name"] = json!("NIGHTLY2");
    v["comments"] = json!("purga");
    let p = plan_of(Some(&cur), v, json!([{"name": "DAYS", "value": "7"}])).unwrap();
    assert_eq!(p.id, "\"APP\".\"NIGHTLY2\"");
    assert_eq!(p.statements.len(), 3);
    assert!(p.statements[0].starts_with("CREATE SCHEDULER JOB \"APP\".\"NIGHTLY2\""));
    assert!(p.statements[1].starts_with("COMMENT ON SCHEDULER JOB \"APP\".\"NIGHTLY2\""));
    assert_eq!(p.statements[2], "DROP SCHEDULER JOB \"APP\".\"NIGHTLY\"");
    assert!(p.script.contains("no cambia el nombre ni el esquema"));
    assert!(p.script.contains("historial"));
    assert!(p.script.contains(IN_TRANSACTION));
    assert_eq!(p.drops, Some(("APP".to_string(), "NIGHTLY".to_string())));

    // Another procedure: drop, then create.
    let mut v = base();
    v["procedure"] = json!("\"APP\".\"OTHER\"");
    let p = plan_of(Some(&cur), v, json!([{"name": "DAYS", "value": "7"}])).unwrap();
    assert_eq!(p.statements.len(), 2);
    assert_eq!(p.statements[0], "DROP SCHEDULER JOB \"APP\".\"NIGHTLY\"");
    assert!(p.statements[1].starts_with("CREATE SCHEDULER JOB \"APP\".\"NIGHTLY\""));
    assert!(p.statements[1].contains("PROCEDURE \"APP\".\"OTHER\""));
    // With a comment, it follows the create (all in one transaction).
    let r = recreate();
    assert_eq!(r.statements.len(), 3);
    assert!(r.statements[2].starts_with("COMMENT ON SCHEDULER JOB \"APP\".\"NIGHTLY\""));
    assert!(p.script.contains("el procedimiento"));
    assert!(p.script.contains(IN_TRANSACTION));
    assert_eq!(p.drops, Some(("APP".to_string(), "NIGHTLY".to_string())));

    // A bound removed, every parameter removed.
    let mut with_from = current();
    with_from.from = Some("2026-11-12 10:00:00".into());
    let p = plan_of(
        Some(&with_from),
        base(),
        json!([{"name": "DAYS", "value": "7"}]),
    )
    .unwrap();
    assert_eq!(p.statements[0], "DROP SCHEDULER JOB \"APP\".\"NIGHTLY\"");
    assert!(p.script.contains("FROM / UNTIL"));
    let p = plan_of(Some(&cur), base(), json!([])).unwrap();
    assert_eq!(p.statements[0], "DROP SCHEDULER JOB \"APP\".\"NIGHTLY\"");
    assert!(p.script.contains("todos los parámetros"));
}

#[test]
fn delete_drops() {
    let p = delete_plan(&current()).unwrap();
    assert_eq!(p.statements, ["DROP SCHEDULER JOB \"APP\".\"NIGHTLY\""]);
    assert!(p
        .script
        .starts_with("-- Borra el scheduler job \"APP\".\"NIGHTLY\""));
}

#[test]
fn every_statement_is_one_piece_of_the_script() {
    let mut v = base();
    v["name"] = json!("J; DROP --");
    v["comments"] = json!("a;\nb -- c /* d");
    let p = plan_of(
        None,
        v,
        json!([{"name": "tag", "value": "x'; DROP TABLE T; --"}]),
    )
    .unwrap();
    assert_eq!(crate::script::split(&p.script).len(), 2);
}

#[test]
fn the_form_reads_the_job() {
    let cat = Catalog {
        schemas: vec!["APP".into(), "OTHER".into()],
        procedures: vec![("APP".into(), "PURGE".into())],
        current: "APP".into(),
    };
    let mut j = current();
    j.from = Some("2026-11-12 10:00:00".into());
    j.params = vec![("tag".into(), "x".into())];
    let f = form_of(Some(&j), &cat, true, None);
    assert!(!f.read_only);
    assert_eq!(f.values["procedure"], json!("\"APP\".\"PURGE\""));
    assert_eq!(f.values["from"], json!("2026-11-12T10:00:00"));
    assert_eq!(f.values["until"], Value::Null);
    assert_eq!(f.values["comments"], json!("purga"));
    assert_eq!(f.values["owner"], json!("ANA"));
    assert_eq!(
        f.choices["procedure"][0],
        ("\"APP\".\"PURGE\"".to_string(), "APP.PURGE".to_string())
    );
    let rows = &f.sections[2].lists[0].rows;
    assert_eq!(rows[0]["name"], json!("tag"));
    for k in ["owner", "valid", "created"] {
        assert!(
            f.sections[0]
                .fields
                .iter()
                .find(|x| x.key == k)
                .unwrap()
                .read_only,
            "{k}"
        );
    }
    // The form as it came saves nothing.
    let values = FormValues {
        values: f.values.clone(),
        lists: BTreeMap::from([("parameters".to_string(), rows.clone())]),
    };
    let w = wanted(&values, true).unwrap();
    let typed = w.typed(&proc_params()).unwrap();
    assert!(plan(Some(&j), &w, &typed, true)
        .unwrap()
        .statements
        .is_empty());

    // A procedure or schema the lists lack still shows.
    let mut j2 = current();
    j2.schema = "HIDDEN".into();
    j2.proc_name = Some("GONE".into());
    let f = form_of(Some(&j2), &cat, true, None);
    assert!(f.choices["schema"].iter().any(|(v, _)| v == "HIDDEN"));
    assert!(f.choices["procedure"]
        .iter()
        .any(|(v, _)| v == "\"APP\".\"GONE\""));

    // A new job: the current schema; no comment field without COMMENTS.
    let f = form_of(None, &cat, false, None);
    assert_eq!(f.values["schema"], json!("APP"));
    assert!(!f.sections[0].fields.iter().any(|x| x.key == "comments"));
    assert!(f.notes.iter().any(|n| n == NO_COMMENTS));

    // HANA's own jobs are read-only.
    let mut sys = current();
    sys.schema = "_SYS_STATISTICS".into();
    assert!(form_of(Some(&sys), &cat, true, None).read_only);
}

// ---------------------------------------------------------------------------
// Saving in one transaction
// ---------------------------------------------------------------------------

/// A connection that records what [`run`] does; statements starting with
/// one of `fail` are refused.
struct Fake {
    calls: std::cell::RefCell<Vec<String>>,
    auto: std::cell::Cell<bool>,
    fail: Vec<&'static str>,
}

impl Fake {
    fn new(auto: bool, fail: &[&'static str]) -> Fake {
        Fake {
            calls: Default::default(),
            auto: std::cell::Cell::new(auto),
            fail: fail.to_vec(),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Conn for Fake {
    async fn sql(&self, stmt: &str) -> Result<()> {
        self.calls.borrow_mut().push(stmt.to_string());
        if self.fail.iter().any(|f| stmt.starts_with(f)) {
            Err(Error::Query(format!("refused: {stmt}")))
        } else {
            Ok(())
        }
    }
    async fn auto_commit(&self) -> bool {
        self.auto.get()
    }
    async fn set_auto(&self, on: bool) {
        self.calls.borrow_mut().push(format!("auto-commit {on}"));
        self.auto.set(on);
    }
    async fn end(&self, commit: bool) -> Result<()> {
        self.sql(if commit { "COMMIT" } else { "ROLLBACK" }).await
    }
}

/// A recreation: the old job dropped, the new one created with its comment.
fn recreate() -> Plan {
    let mut v = base();
    v["procedure"] = json!("\"APP\".\"OTHER\"");
    v["comments"] = json!("purga");
    plan_of(Some(&current()), v, json!([{"name": "DAYS", "value": "7"}])).unwrap()
}

#[tokio::test]
async fn several_statements_run_in_one_transaction() {
    let p = recreate();
    let c = Fake::new(true, &[]);
    run(&c, &p).await.unwrap();
    let mut want = vec![
        "auto-commit false".to_string(),
        "SET TRANSACTION AUTOCOMMIT DDL OFF".to_string(),
    ];
    want.extend(p.statements.iter().cloned());
    want.extend([
        "COMMIT".to_string(),
        "SET TRANSACTION AUTOCOMMIT DDL ON".to_string(),
        "auto-commit true".to_string(),
    ]);
    assert_eq!(c.calls(), want);
    assert!(c.auto.get());
}

#[tokio::test]
async fn a_failed_create_rolls_the_drop_back() {
    let p = recreate();
    let c = Fake::new(true, &["CREATE SCHEDULER JOB"]);
    let e = run(&c, &p).await.unwrap_err().to_string();
    assert!(
        e.contains("sentencia 2 de 3") && e.contains("no cambió nada"),
        "{e}"
    );
    assert!(e.contains("refused: CREATE"), "{e}");
    assert_eq!(
        c.calls(),
        [
            "auto-commit false",
            "SET TRANSACTION AUTOCOMMIT DDL OFF",
            "DROP SCHEDULER JOB \"APP\".\"NIGHTLY\"",
            &p.statements[1],
            "ROLLBACK",
            "SET TRANSACTION AUTOCOMMIT DDL ON",
            "auto-commit true",
        ]
    );
    // A connection that had auto-commit off gets it back off.
    let c = Fake::new(false, &["COMMENT ON"]);
    assert!(run(&c, &p).await.is_err());
    assert_eq!(c.calls().last().unwrap(), "auto-commit false");
    assert!(c.calls().contains(&"ROLLBACK".to_string()));
    // A refused rollback is said.
    let c = Fake::new(true, &["CREATE SCHEDULER JOB", "ROLLBACK"]);
    let e = run(&c, &p).await.unwrap_err().to_string();
    assert!(e.contains("no se pudieron deshacer"), "{e}");
    // A refused commit is rolled back.
    let c = Fake::new(true, &["COMMIT"]);
    let e = run(&c, &p).await.unwrap_err().to_string();
    assert!(e.contains("no confirmó"), "{e}");
    assert!(c.calls().contains(&"ROLLBACK".to_string()));
}

#[tokio::test]
async fn without_autocommit_ddl_off_each_statement_commits_alone() {
    let p = recreate();
    let c = Fake::new(true, &["SET TRANSACTION AUTOCOMMIT DDL OFF"]);
    run(&c, &p).await.unwrap();
    let mut want = vec![
        "auto-commit false".to_string(),
        "SET TRANSACTION AUTOCOMMIT DDL OFF".to_string(),
        "auto-commit true".to_string(),
    ];
    want.extend(p.statements.iter().cloned());
    assert_eq!(c.calls(), want);
    let c = Fake::new(true, &["SET TRANSACTION AUTOCOMMIT DDL OFF", "CREATE"]);
    let e = run(&c, &p).await.unwrap_err().to_string();
    assert!(e.contains("cada una se confirmó sola"), "{e}");
}

#[tokio::test]
async fn one_statement_runs_as_it_is() {
    let p = delete_plan(&current()).unwrap();
    let c = Fake::new(true, &[]);
    run(&c, &p).await.unwrap();
    assert_eq!(c.calls(), ["DROP SCHEDULER JOB \"APP\".\"NIGHTLY\""]);
    let c = Fake::new(true, &["DROP"]);
    assert_eq!(
        run(&c, &p).await.unwrap_err().to_string(),
        Error::Query("refused: DROP SCHEDULER JOB \"APP\".\"NIGHTLY\"".into()).to_string()
    );
}

// ---------------------------------------------------------------------------
// What the login may do
// ---------------------------------------------------------------------------

fn set(cols: &[&str], rows: &[&[Option<&str>]]) -> Set {
    Set {
        cols: cols.iter().map(|c| c.to_string()).collect(),
        rows: rows
            .iter()
            .map(|r| r.iter().map(|v| v.map(str::to_string)).collect())
            .collect(),
    }
}

fn rights(user: &str, owned: &[&str], privileges: &[&[Option<&str>]]) -> Rights {
    let owned: Vec<Vec<Option<&str>>> = owned.iter().map(|s| vec![Some(*s)]).collect();
    let owned: Vec<&[Option<&str>]> = owned.iter().map(Vec::as_slice).collect();
    Rights::of(
        user,
        &set(&["SCHEMA_NAME"], &owned),
        &set(
            &["PRIVILEGE", "OBJECT_TYPE", "SCHEMA_NAME", "OBJECT_NAME"],
            privileges,
        ),
    )
}

#[test]
fn privileges_say_what_the_login_may_do() {
    let job = current(); // APP.NIGHTLY, owned by ANA
                         // Its owner.
    let ana = rights("ANA", &["ANA"], &[]);
    assert!(ana.alters(&job) && ana.drops(&job));
    assert!(ana.creates() && ana.create_in("ANA") && !ana.create_in("APP"));
    // Another user who only reads the catalog.
    let bob = rights("BOB", &["BOB"], &[]);
    assert!(!bob.alters(&job) && !bob.drops(&job));
    // ALTER on the job, DROP on its schema; CREATE ANY on APP.
    let carl = rights(
        "CARL",
        &[],
        &[
            &[
                Some("ALTER"),
                Some("SCHEDULERJOB"),
                Some("APP"),
                Some("NIGHTLY"),
            ],
            &[Some("DROP"), Some("SCHEMA"), Some("APP"), None],
            &[Some("CREATE ANY"), Some("SCHEMA"), None, Some("APP")],
            &[Some("ALTER"), Some("TABLE"), Some("APP"), Some("OTHER")],
        ],
    );
    assert!(carl.alters(&job) && carl.drops(&job));
    assert!(carl.create_in("APP") && carl.creates() && !carl.create_in("X"));
    let mut other = current();
    other.name = "OTHER2".into();
    assert!(!carl.alters(&other) && carl.drops(&other));
    // The owner of the schema; DATA ADMIN.
    assert!(rights("DAN", &["APP"], &[]).alters(&job));
    let admin = rights(
        "EVE",
        &[],
        &[&[Some("DATA ADMIN"), Some("SYSTEMPRIVILEGE"), None, None]],
    );
    assert!(admin.alters(&job) && admin.create_in("ANY") && admin.changes_any());
    // A restricted user: no schema, no grants.
    let none = rights("RUSER", &[], &[]);
    assert!(!none.creates() && !none.changes_any());
}

#[test]
fn the_form_and_the_save_follow_the_privileges() {
    let cat = Catalog {
        schemas: vec!["APP".into(), "BOB".into(), "OTHER".into()],
        procedures: vec![("APP".into(), "PURGE".into())],
        current: "APP".into(),
    };
    let job = current();
    let bob = rights("BOB", &["BOB"], &[]);
    // Someone else's job: read-only, with the note.
    let f = form_of(Some(&job), &cat, true, Some(&bob));
    assert!(f.read_only);
    assert_eq!(f.notes[0], NO_ALTER);
    // A new job: only where the login may create (the current schema
    // isn't one, so the first that is).
    let f = form_of(None, &cat, true, Some(&bob));
    assert!(!f.read_only);
    assert_eq!(
        f.choices["schema"],
        [("BOB".to_string(), "BOB".to_string())]
    );
    assert_eq!(f.values["schema"], json!("BOB"));
    // No schema to create in: read-only.
    let f = form_of(None, &cat, true, Some(&rights("RUSER", &[], &[])));
    assert!(f.read_only && f.notes[0] == NO_CREATE_ANY);
    // Unknown privileges: everything offered.
    assert!(!form_of(Some(&job), &cat, true, None).read_only);
    assert_eq!(form_of(None, &cat, true, None).choices["schema"].len(), 3);

    // The save refuses the same.
    let w = wanted(&fv(base(), json!([])), true).unwrap();
    let new = plan(None, &w, &[], true).unwrap();
    assert!(check_rights(None, None, &w, &new).is_ok());
    let e = check_rights(Some(&bob), None, &w, &new)
        .unwrap_err()
        .to_string();
    assert!(e.contains("CREATE ANY en el esquema \"APP\""), "{e}");
    let ana = rights("ANA", &["ANA"], &[]);
    let mut v = base();
    v["cron"] = json!("* * * * 4 0 0");
    let w = wanted(&fv(v, json!([{"name": "DAYS", "value": "7"}])), true).unwrap();
    let alter = plan(Some(&job), &w, &[("DAYS".into(), "7".into())], false).unwrap();
    assert!(check_rights(Some(&ana), Some(&job), &w, &alter).is_ok());
    let e = check_rights(Some(&bob), Some(&job), &w, &alter)
        .unwrap_err()
        .to_string();
    assert_eq!(e, Error::Query(NO_ALTER.into()).to_string());
    // A recreation also needs DROP, and CREATE ANY where the job goes.
    let p = recreate();
    let w = wanted(
        &fv(
            json!({"schema": "APP", "name": "NIGHTLY", "procedure": "\"APP\".\"OTHER\"", "cron": "* * * * 3 0 0"}),
            json!([]),
        ),
        true,
    )
    .unwrap();
    let alter_only = rights(
        "CARL",
        &[],
        &[&[
            Some("ALTER"),
            Some("SCHEDULERJOB"),
            Some("APP"),
            Some("NIGHTLY"),
        ]],
    );
    let e = check_rights(Some(&alter_only), Some(&job), &w, &p)
        .unwrap_err()
        .to_string();
    assert!(e.contains("no tiene DROP"), "{e}");
    // The job's owner without CREATE ANY in APP (ANA owns only ANA).
    let e = check_rights(Some(&ana), Some(&job), &w, &p)
        .unwrap_err()
        .to_string();
    assert!(e.contains("CREATE ANY"), "{e}");
    assert!(check_rights(Some(&rights("DAN", &["APP"], &[])), Some(&job), &w, &p).is_ok());
}
