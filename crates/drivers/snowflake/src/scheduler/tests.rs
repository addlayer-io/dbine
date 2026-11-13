//! Unit tests of the scheduler's scripts, plans and forms (no server: a
//! fake runner answers the SHOW statements and records what runs).

use super::alert::{self, AlertRow};
use super::graph::{self, GraphRow};
use super::logs;
use super::task;
use super::*;
use dbine_driver::FormList;
use serde_json::json;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// A fake server
// ---------------------------------------------------------------------------

struct Fake {
    wh: Option<String>,
    answers: Vec<(String, Set)>,
    /// Statements that fail (by a piece of their text).
    fail: Vec<String>,
    /// Statements that fail with the server's own message (piece, message).
    errors: Vec<(String, String)>,
    seen: Mutex<Vec<String>>,
}

impl Fake {
    fn new(answers: Vec<(&str, Set)>) -> Fake {
        Fake {
            wh: Some("WH".into()),
            answers: answers
                .into_iter()
                .map(|(k, s)| (k.to_string(), s))
                .collect(),
            fail: Vec::new(),
            errors: Vec::new(),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

#[dbine_driver::async_trait]
impl Runner for Fake {
    async fn rows(&self, sql: &str) -> Result<Set> {
        self.seen.lock().unwrap().push(sql.to_string());
        if let Some(f) = self.fail.iter().find(|f| sql.contains(f.as_str())) {
            return Err(Error::Query(format!(
                "SQL compilation error: rechazado ({f})"
            )));
        }
        if let Some((_, m)) = self.errors.iter().find(|(f, _)| sql.contains(f.as_str())) {
            return Err(Error::Query(m.clone()));
        }
        Ok(self
            .answers
            .iter()
            .find(|(n, _)| sql.contains(n.as_str()))
            .map(|(_, s)| s.clone())
            .unwrap_or_default())
    }
    fn warehouse(&self) -> Option<String> {
        self.wh.clone()
    }
    fn database(&self) -> Option<String> {
        Some("DB".into())
    }
}

fn set(cols: &[&str], rows: &[&[&str]]) -> Set {
    Set {
        cols: cols.iter().map(|c| c.to_string()).collect(),
        rows: rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|c| (!c.is_empty()).then(|| c.to_string()))
                    .collect()
            })
            .collect(),
    }
}

const TASK_COLS: [&str; 18] = [
    "database_name",
    "schema_name",
    "name",
    "id",
    "owner",
    "owner_role_type",
    "comment",
    "warehouse",
    "schedule",
    "predecessors",
    "state",
    "definition",
    "condition",
    "overlap_policy",
    "error_integration",
    "config",
    "execute_as_user",
    "task_relations",
];

/// A row of SHOW TASKS: name, warehouse, schedule, predecessors, state,
/// definition, condition, relations.
#[allow(clippy::too_many_arguments)]
fn task_row<'a>(
    name: &'a str,
    wh: &'a str,
    schedule: &'a str,
    preds: &'a str,
    state: &'a str,
    def: &'a str,
    cond: &'a str,
    rel: &'a str,
) -> Vec<&'a str> {
    let root = preds == "[]" && !rel.contains("Finalized");
    vec![
        "DB",
        "ETL",
        name,
        "01b2-id",
        "SYSADMIN",
        "ROLE",
        if root { "c" } else { "" },
        wh,
        schedule,
        preds,
        state,
        def,
        cond,
        if root { "NO_OVERLAP" } else { "" },
        if root { "NOTIFY" } else { "" },
        if root { r#"{"a": 1}"# } else { "" },
        "",
        rel,
    ]
}

fn tasks_set(root_rel: &str) -> Set {
    let rows: Vec<Vec<&str>> = vec![
        task_row(
            "ROOT",
            "WH",
            "USING CRON 0 3 * * * UTC",
            "[]",
            "started",
            "SELECT 1",
            "",
            root_rel,
        ),
        task_row(
            "LOAD",
            "",
            "",
            r#"["DB.ETL.ROOT"]"#,
            "started",
            "INSERT INTO t SELECT 1",
            "",
            r#"{"Predecessors":["DB.ETL.ROOT"]}"#,
        ),
        task_row(
            "REPORT",
            "WH",
            "",
            r#"["DB.ETL.LOAD"]"#,
            "suspended",
            "CALL p()",
            "SYSTEM$GET_PREDECESSOR_RETURN_VALUE('LOAD') = 'ok'",
            r#"{"Predecessors":["DB.ETL.LOAD"]}"#,
        ),
        task_row(
            "FIN",
            "",
            "",
            "[]",
            "started",
            "SELECT 2",
            "",
            if root_rel.contains("TaskGraph") {
                r#"{"FinalizedGraph":"DB.ETL.PIPE","Predecessors":[]}"#
            } else {
                r#"{"FinalizedRootTask":"DB.ETL.ROOT","Predecessors":[]}"#
            },
        ),
        task_row(
            "OTHER",
            "WH",
            "5 MINUTE",
            "[]",
            "suspended",
            "SELECT 3",
            "",
            r#"{"Predecessors":[]}"#,
        ),
    ];
    let refs: Vec<&[&str]> = rows.iter().map(Vec::as_slice).collect();
    set(&TASK_COLS, &refs)
}

fn params_set(rows: &[(&str, &str, &str)]) -> Set {
    let mut all: Vec<Vec<&str>> = rows
        .iter()
        .map(|(k, v, t)| vec![*k, *v, "", "TASK", *t])
        .collect();
    // Inherited ones don't count.
    all.push(vec!["LOG_LEVEL", "OFF", "OFF", "", "STRING"]);
    let refs: Vec<&[&str]> = all.iter().map(Vec::as_slice).collect();
    set(&["key", "value", "default", "level", "type"], &refs)
}

fn server(root_rel: &str, owner_held: bool) -> Fake {
    Fake::new(vec![
        ("SHOW TASKS IN SCHEMA", tasks_set(root_rel)),
        (
            r#"SHOW PARAMETERS IN TASK "DB"."ETL"."ROOT""#,
            params_set(&[
                ("USER_TASK_TIMEOUT_MS", "60000", "NUMBER"),
                ("TIMEZONE", "UTC", "STRING"),
            ]),
        ),
        (
            r#"SHOW PARAMETERS IN TASK "DB"."ETL"."LOAD""#,
            params_set(&[
                (
                    "USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE",
                    "XSMALL",
                    "STRING",
                ),
                ("QUERY_TAG", "x", "STRING"),
            ]),
        ),
        ("SHOW PARAMETERS IN TASK", params_set(&[])),
        (
            "SHOW WAREHOUSES",
            set(
                &["name", "state"],
                &[&["WH", "STARTED"], &["BIG_WH", "SUSPENDED"]],
            ),
        ),
        (
            "SHOW NOTIFICATION INTEGRATIONS",
            set(&["name"], &[&["NOTIFY"]]),
        ),
        (
            "SHOW PARAMETERS IN SESSION",
            set(
                &["key", "value", "default", "level", "type"],
                &[
                    &["TIMEZONE", "UTC", "UTC", "", "STRING"],
                    &["AUTOCOMMIT", "true", "true", "", "BOOLEAN"],
                    &["QUERY_TAG", "", "", "", "STRING"],
                    &["STATEMENT_TIMEOUT_IN_SECONDS", "0", "0", "", "NUMBER"],
                    &["USER_TASK_TIMEOUT_MS", "3600000", "3600000", "", "NUMBER"],
                ],
            ),
        ),
        (
            "IS_ROLE_IN_SESSION",
            set(&["r0"], &[&[if owner_held { "true" } else { "false" }]]),
        ),
    ])
}

const ROOT_ID: &str = r#""DB"."ETL"."ROOT""#;

/// What the designer sends back for a form as it came.
fn values_of(f: &ObjectForm) -> FormValues {
    let lists: BTreeMap<String, Vec<BTreeMap<String, Value>>> = f
        .sections
        .iter()
        .flat_map(|s| s.lists.iter())
        .map(|l: &FormList| (l.key.clone(), l.rows.clone()))
        .collect();
    FormValues {
        values: f.values.clone(),
        lists,
    }
}

fn map(v: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

fn fv(values: Value, lists: Value) -> FormValues {
    FormValues {
        values: map(values),
        lists: serde_json::from_value(lists).unwrap(),
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
    put("warehouse", &["WH", "BIG_WH"]);
    put("notification_integration", &["NOTIFY"]);
    put(
        "session_parameter",
        &[
            "TIMEZONE",
            "AUTOCOMMIT",
            "QUERY_TAG",
            "STATEMENT_TIMEOUT_IN_SECONDS",
        ],
    );
    for (k, t) in [
        ("TIMEZONE", "STRING"),
        ("AUTOCOMMIT", "BOOLEAN"),
        ("QUERY_TAG", "STRING"),
        ("STATEMENT_TIMEOUT_IN_SECONDS", "NUMBER"),
    ] {
        c.param_types.insert(k.into(), t.into());
    }
    c
}

fn row(v: &mut FormValues, i: usize) -> &mut BTreeMap<String, Value> {
    &mut v.lists.get_mut("tasks").unwrap()[i]
}

fn err_of<T: std::fmt::Debug>(r: Result<T>) -> String {
    r.unwrap_err().to_string()
}

async fn edit_plan(f: &Fake, change: impl FnOnce(&mut FormValues)) -> Result<Plan> {
    let form = task::form(f, Some(ROOT_ID)).await.unwrap();
    let mut v = values_of(&form);
    change(&mut v);
    task::plan(f, Some(ROOT_ID), &v).await
}

fn row_mut<'a>(v: &'a mut FormValues, name: &str) -> &'a mut BTreeMap<String, Value> {
    v.lists
        .get_mut("tasks")
        .unwrap()
        .iter_mut()
        .find(|r| r.get("name") == Some(&json!(name)))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Helpers: names, schedules, code, parameters
// ---------------------------------------------------------------------------

#[test]
fn names_read_as_snowflake_reads_identifiers() {
    assert_eq!(ident("my_task", "N").unwrap(), "MY_TASK");
    assert_eq!(ident(" t$1 ", "N").unwrap(), "T$1");
    assert_eq!(ident(r#""my task""#, "N").unwrap(), "my task");
    assert_eq!(ident(r#""a""b""#, "N").unwrap(), r#"a"b"#);
    for bad_name in [
        "", "1abc", "a-b", "a b", r#""a"#, r#""a"b"#, r#""""#, "a;DROP",
    ] {
        assert!(ident(bad_name, "N").is_err(), "{bad_name}");
    }
    assert_eq!(shown_ident("LOAD"), "LOAD");
    assert_eq!(shown_ident("myTask"), r#""myTask""#);
    assert_eq!(shown_ident("A B"), r#""A B""#);
    for n in ["LOAD", "myTask", "A B", r#"a"b"#] {
        assert_eq!(ident(&shown_ident(n), "N").unwrap(), n);
    }
    assert_eq!(
        parse_schema(r#""DB"."My Schema""#),
        Some(("DB".into(), "My Schema".into()))
    );
    assert_eq!(parse_schema("DB.S"), None);
    assert_eq!(parse_schema(r#""DB"."S"; DROP"#), None);
    assert_eq!(
        relation_id("MY_DB.MY_SCHEMA.MY_PIPE"),
        Some(r#""MY_DB"."MY_SCHEMA"."MY_PIPE""#.into())
    );
}

#[test]
fn schedules_parse_and_write_as_snowflake_shows_them() {
    assert_eq!(
        Schedule::parse("USING CRON 0 3 * * * UTC"),
        Some(Schedule::Cron("0 3 * * *".into(), "UTC".into()))
    );
    assert_eq!(
        Schedule::parse("5 MINUTE"),
        Some(Schedule::Every(5, Unit::Minutes))
    );
    assert_eq!(
        Schedule::parse("90 seconds"),
        Some(Schedule::Every(90, Unit::Seconds))
    );
    assert_eq!(
        Schedule::parse("2 H"),
        Some(Schedule::Every(2, Unit::Hours))
    );
    assert_eq!(Schedule::parse("tomorrow"), None);
    assert_eq!(Schedule::Every(5, Unit::Minutes).text(false), "5 MINUTES");
    assert_eq!(Schedule::Every(5, Unit::Minutes).text(true), "5 MINUTE");
    assert_eq!(
        Schedule::Cron("0 3 * * *".into(), "UTC".into()).text(false),
        "USING CRON 0 3 * * * UTC"
    );
    assert_eq!(cron_fields("0  3 L * 5L", "c").unwrap(), "0 3 L * 5L");
    assert!(cron_fields("0 3 * *", "c").is_err());
    assert!(cron_fields("0 3 * * * '", "c").is_err());
    assert!(cron_fields("0 3 * * ';", "c").is_err());
    assert_eq!(
        time_zone("America/Argentina/Buenos_Aires").unwrap(),
        "America/Argentina/Buenos_Aires"
    );
    assert!(time_zone("UTC'").is_err());
    assert!(time_zone("").is_err());
    let v = map(json!({"schedule": "interval", "every": 5, "unit": "SECONDS"}));
    assert!(err_of(schedule_of(Vals(&v), false)).contains("entre 10 y 691200"));
    let v = map(json!({"schedule": "interval", "every": "9", "unit": "HOURS"}));
    assert_eq!(
        schedule_of(Vals(&v), false).unwrap(),
        Some(Schedule::Every(9, Unit::Hours))
    );
    let v = map(json!({"schedule": "interval", "every": 20000}));
    assert!(schedule_of(Vals(&v), true).is_err());
    let v = map(json!({"schedule": "none"}));
    assert_eq!(schedule_of(Vals(&v), false).unwrap(), None);
    let v = map(json!({"schedule": "weekly"}));
    assert!(schedule_of(Vals(&v), false).is_err());
    assert_eq!(
        completion_interval("10 minutes", "t").unwrap().as_deref(),
        Some("10 MINUTES")
    );
    assert!(completion_interval("5 SECONDS", "t").is_err());
    assert!(completion_interval("25 HOURS", "t").is_err());
    assert_eq!(completion_interval("", "t").unwrap(), None);
}

#[test]
fn code_is_one_statement() {
    assert_eq!(one_statement("  SELECT 1;  ", "SQL").unwrap(), "SELECT 1");
    assert_eq!(
        one_statement(
            "BEGIN\n  INSERT INTO t VALUES (1);\n  DELETE FROM t;\nEND;",
            "SQL"
        )
        .unwrap(),
        "BEGIN\n  INSERT INTO t VALUES (1);\n  DELETE FROM t;\nEND"
    );
    assert_eq!(
        one_statement("EXECUTE IMMEDIATE $$ SELECT 1; SELECT 2; $$", "SQL").unwrap(),
        "EXECUTE IMMEDIATE $$ SELECT 1; SELECT 2; $$"
    );
    assert!(err_of(one_statement("SELECT 1; DROP TABLE t", "SQL")).contains("una sola instrucción"));
    assert!(one_statement("  ;  ", "SQL").is_err());
    assert_eq!(expression("", "W").unwrap(), None);
    assert!(expression("TRUE; DROP TABLE t", "W").is_err());
    assert_eq!(
        json_object(r#" {"a": 1} "#, "C").unwrap().as_deref(),
        Some(r#"{"a": 1}"#)
    );
    assert!(json_object("[1]", "C").is_err());
    assert!(json_object("{a:1}", "C").is_err());
    assert!(same_json(
        Some(r#"{"a":1,"b":2}"#),
        Some(r#"{ "b": 2, "a": 1 }"#)
    ));
    assert!(!same_json(Some(r#"{"a":1}"#), None));
}

#[test]
fn parameters_are_written_by_their_type() {
    assert_eq!(param_value("T", "UTC", Some("STRING")).unwrap(), "'UTC'");
    assert_eq!(param_value("T", "it's", None).unwrap(), "'it''s'");
    assert_eq!(param_value("N", "60", Some("NUMBER")).unwrap(), "60");
    assert!(param_value("N", "60; DROP", Some("NUMBER")).is_err());
    assert_eq!(param_value("B", "true", Some("BOOLEAN")).unwrap(), "TRUE");
    assert!(param_value("B", "yes", Some("BOOLEAN")).is_err());
    let c = choices();
    let known = HashMap::new();
    assert_eq!(
        params_text(
            "TIMEZONE = 'America/Bogota'\n\nautocommit = true",
            &c,
            &known
        )
        .unwrap(),
        vec![
            ("TIMEZONE".to_string(), "'America/Bogota'".to_string()),
            ("AUTOCOMMIT".to_string(), "TRUE".to_string())
        ]
    );
    assert!(err_of(params_text("NOT_A_PARAM = 1", &c, &known)).contains("no es un parámetro"));
    assert!(err_of(params_text("LOG_LEVEL = 'INFO'", &c, &known)).contains("propio campo"));
    assert!(params_text("TIMEZONE 'UTC'", &c, &known).is_err());
    assert!(params_text("TIMEZONE = 'a'\nTIMEZONE = 'b'", &c, &known).is_err());
    assert!(params_text("TIME-ZONE = 'a'", &c, &known).is_err());
    assert_eq!(unliteral("'a''b\\\\c'"), "a'b\\c");
    assert_eq!(unliteral("60"), "60");
}

#[test]
fn a_script_ends_each_statement_and_keeps_line_comments_apart() {
    let mut p = Plan::new(None, "Prueba");
    p.push("ALTER TASK t SUSPEND".into());
    p.push("CREATE TASK t\nAS\nSELECT 1 -- note".into());
    let s = p.script();
    assert!(s.starts_with("-- Prueba\n"), "{s}");
    assert!(s.contains("ALTER TASK t SUSPEND;\n"), "{s}");
    assert!(s.contains("SELECT 1 -- note\n;\n"), "{s}");
    assert!(Plan::new(None, "x").script().contains("No hay cambios"));
}

#[tokio::test]
async fn futures_run_together_in_order() {
    let futs: Vec<_> = (0..20)
        .map(|i| async move {
            tokio::task::yield_now().await;
            i * 2
        })
        .collect();
    assert_eq!(
        together(futs).await,
        (0..20).map(|i| i * 2).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn info_lists_the_three_kinds() {
    let f = Fake::new(vec![("CURRENT_ROLE", set(&["role"], &[&["SYSADMIN"]]))]);
    let i = info(&f).await.unwrap();
    let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "alert", "task_graph"]);
    assert!(i
        .kinds
        .iter()
        .all(|k| k.can_create && k.can_edit && k.can_delete));
    assert!(i.logs);
    assert!(i.notes.iter().any(|n| n.contains("algún privilegio")));
    assert!(i.notes.iter().any(|n| n == PREVIEW));
    assert!(f.seen().iter().any(|s| s == GRAPHS_PROBE), "{:?}", f.seen());
    assert!(err_of(items(&f, "operator").await).contains("no es un tipo"));
}

#[tokio::test]
async fn info_leaves_out_task_graphs_where_the_account_has_none() {
    for msg in [
        "SQL compilation error:\nsyntax error line 1 at position 5 unexpected 'TASK'. (SQLSTATE 42000)",
        "Unsupported feature 'TASK GRAPH'. (SQLSTATE 0A000)",
    ] {
        let mut f = Fake::new(vec![("CURRENT_ROLE", set(&["role"], &[&["SYSADMIN"]]))]);
        f.errors = vec![("SHOW TASK GRAPHS".into(), msg.into())];
        let i = info(&f).await.unwrap();
        let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(keys, ["job", "alert"], "{msg}");
        assert_eq!(i.notes, [VISIBLE_ONLY_NO_GRAPHS, NO_GRAPHS], "{msg}");
    }
}

#[tokio::test]
async fn info_keeps_task_graphs_the_role_cant_list_or_a_failure_hides() {
    let mut f = Fake::new(vec![("CURRENT_ROLE", set(&["role"], &[&["ACCOUNTADMIN"]]))]);
    f.errors = vec![(
        "SHOW TASK GRAPHS".into(),
        "SQL access control error:\nInsufficient privileges to operate on account 'X' (SQLSTATE 42501)".into(),
    )];
    let i = info(&f).await.unwrap();
    let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "alert", "task_graph"]);
    assert!(i.kinds[2].can_create && i.kinds[2].can_edit && i.kinds[2].can_delete);
    assert_eq!(i.notes, [GRAPHS_DENIED, PREVIEW]);
    // A failure that says nothing about the account keeps the kind as it
    // was: its list says what happened.
    f.errors = vec![("SHOW TASK GRAPHS".into(), "HTTP 503".into())];
    let i = info(&f).await.unwrap();
    assert_eq!(i.kinds.len(), 3);
    assert_eq!(i.notes, [PREVIEW]);
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

fn new_graph_values() -> FormValues {
    fv(
        json!({
            "schema": r#""DB"."ETL""#, "name": "root", "compute": "warehouse", "warehouse": "WH",
            "schedule": "cron", "cron": "0  3 * * *", "timezone": "UTC",
            "comment": "it's a \\ test", "definition": "SELECT 1;", "enabled": true,
            "overlap": "ALLOW_CHILD_OVERLAP", "config": "{\"a\": 1}", "auto_retry": 2,
            "suspend_after": "3", "timeout_ms": 60000, "log_level": "INFO",
            "error_integration": "notify", "execute_as": "etl_user"
        }),
        json!({
            "session_params": [
                {"name": "TIMEZONE", "value": "America/Los_Angeles"},
                {"name": "STATEMENT_TIMEOUT_IN_SECONDS", "value": "60"}
            ],
            "tasks": [
                {"name": "load", "role": "child", "after_root": true, "after": [],
                 "compute": "serverless", "initial_size": "XSMALL", "trace_level": "ON_EVENT",
                 "definition": "INSERT INTO t SELECT 1", "enabled": true, "session": "QUERY_TAG = 'x'"},
                {"name": "\"Report\"", "role": "child", "after_root": false, "after": ["LOAD"],
                 "compute": "warehouse", "warehouse": "WH", "definition": "CALL p()",
                 "condition": "SYSTEM$GET_PREDECESSOR_RETURN_VALUE('LOAD') = 'ok'", "enabled": false},
                {"name": "fin", "role": "finalizer", "compute": "serverless", "definition": "SELECT 2",
                 "enabled": true}
            ]
        }),
    )
}

#[test]
fn a_new_graph_is_created_root_first_and_resumed_root_last() {
    let s = task::spec(&new_graph_values(), &choices(), None).unwrap();
    let p = task::create_plan(&s).unwrap();
    assert_eq!(p.id.as_deref(), Some(ROOT_ID));
    assert_eq!(
        p.steps[0],
        r#"CREATE TASK "DB"."ETL"."ROOT"
  WAREHOUSE = "WH"
  SCHEDULE = 'USING CRON 0 3 * * * UTC'
  CONFIG = '{"a": 1}'
  OVERLAP_POLICY = ALLOW_CHILD_OVERLAP
  TIMEZONE = 'America/Los_Angeles', STATEMENT_TIMEOUT_IN_SECONDS = 60
  USER_TASK_TIMEOUT_MS = 60000
  SUSPEND_TASK_AFTER_NUM_FAILURES = 3
  ERROR_INTEGRATION = "NOTIFY"
  LOG_LEVEL = 'INFO'
  COMMENT = 'it''s a \\ test'
  TASK_AUTO_RETRY_ATTEMPTS = 2
  EXECUTE AS USER "ETL_USER"
AS
SELECT 1"#
    );
    assert_eq!(
        p.steps[1],
        r#"CREATE TASK "DB"."ETL"."LOAD"
  USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE = 'XSMALL'
  TRACE_LEVEL = 'ON_EVENT', QUERY_TAG = 'x'
  AFTER "DB"."ETL"."ROOT"
AS
INSERT INTO t SELECT 1"#
    );
    assert_eq!(
        p.steps[2],
        r#"CREATE TASK "DB"."ETL"."Report"
  WAREHOUSE = "WH"
  AFTER "DB"."ETL"."LOAD"
  WHEN SYSTEM$GET_PREDECESSOR_RETURN_VALUE('LOAD') = 'ok'
AS
CALL p()"#
    );
    assert_eq!(
        p.steps[3],
        "CREATE TASK \"DB\".\"ETL\".\"FIN\"\n  FINALIZE = \"DB\".\"ETL\".\"ROOT\"\nAS\nSELECT 2"
    );
    assert_eq!(
        &p.steps[4..],
        [
            r#"ALTER TASK "DB"."ETL"."LOAD" RESUME"#,
            r#"ALTER TASK "DB"."ETL"."FIN" RESUME"#,
            r#"ALTER TASK "DB"."ETL"."ROOT" RESUME"#,
        ]
    );
    assert!(p.restore.is_none());
    let script = p.script();
    assert!(script.contains("(nueva)"), "{script}");
    assert!(script.contains("crea las tareas suspendidas"), "{script}");
}

#[test]
fn a_new_graph_is_checked() {
    let c = choices();
    let with = |f: &dyn Fn(&mut FormValues)| {
        let mut v = new_graph_values();
        f(&mut v);
        task::spec(&v, &c, None)
    };
    assert!(err_of(with(&|v| {
        row(v, 0).insert("after".into(), json!(["\"Report\""]));
        row(v, 0).insert("after_root".into(), json!(false));
    }))
    .contains("ciclo"));
    assert!(err_of(with(&|v| {
        row(v, 1).insert("after".into(), json!([]));
    }))
    .contains("al menos una tarea"));
    assert!(err_of(with(&|v| {
        row(v, 1).insert("role".into(), json!("finalizer"));
    }))
    .contains("un solo finalizador"));
    assert!(err_of(with(&|v| {
        row(v, 1).insert("after".into(), json!(["FIN"]));
    }))
    .contains("finalizador"));
    assert!(err_of(with(&|v| {
        row(v, 1).insert("after".into(), json!(["NOPE"]));
    }))
    .contains("no es del grafo"));
    assert!(err_of(with(&|v| {
        row(v, 1).insert("name".into(), json!("load"));
    }))
    .contains("dos tareas"));
    assert!(err_of(with(&|v| {
        row(v, 0).insert("definition".into(), json!("SELECT 1; DROP TABLE t"));
    }))
    .contains("una sola instrucción"));
    assert!(err_of(with(&|v| {
        v.values.insert("warehouse".into(), json!("NOT_MINE"));
    }))
    .contains("no está entre"));
    assert!(err_of(with(&|v| {
        v.values.insert("error_integration".into(), json!("OTHER"));
    }))
    .contains("no está entre"));
    assert!(err_of(with(&|v| {
        v.values.insert("cron".into(), json!("0 3 * *"));
    }))
    .contains("cinco campos"));
    assert!(err_of(with(&|v| {
        v.values.insert("schema".into(), json!("DB.ETL"));
    }))
    .contains("no es un esquema"));
    assert!(err_of(with(&|v| {
        v.values.insert("auto_retry".into(), json!(31));
    }))
    .contains("entre 0 y 30"));
    assert!(err_of(with(&|v| {
        v.lists.get_mut("session_params").unwrap()[0]
            .insert("name".into(), json!("USER_TASK_TIMEOUT_MS"));
    }))
    .contains("propio campo"));
    assert!(err_of(with(&|v| {
        row(v, 0).insert("min_size".into(), json!("LARGE"));
    }))
    .contains("mínimo"));
    assert!(err_of(with(&|v| {
        v.values.insert("name".into(), json!("a b"));
    }))
    .contains("comillas"));
}

#[tokio::test]
async fn a_new_task_needs_a_free_name() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let mut v = new_graph_values();
    v.values.insert("name".into(), json!("other"));
    let e = err_of(task::plan(&f, None, &v).await);
    assert!(e.contains("Ya existe una tarea «OTHER»"), "{e}");
    // Nothing but reads ran.
    assert!(f
        .seen()
        .iter()
        .all(|s| s.starts_with("SHOW") || s.starts_with("SELECT")));
}

#[tokio::test]
async fn the_form_reads_the_graph_and_saves_nothing_when_untouched() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let form = task::form(&f, Some(ROOT_ID)).await.unwrap();
    assert!(!form.read_only, "{:?}", form.notes);
    let v = &form.values;
    assert_eq!(v["name"], json!("ROOT"));
    assert_eq!(v["schedule"], json!("cron"));
    assert_eq!(v["cron"], json!("0 3 * * *"));
    assert_eq!(v["compute"], json!("warehouse"));
    assert_eq!(v["timeout_ms"], json!(60000));
    assert_eq!(v["error_integration"], json!("NOTIFY"));
    let lists = values_of(&form).lists;
    assert_eq!(
        lists["session_params"],
        vec![map(json!({"name": "TIMEZONE", "value": "UTC"}))]
    );
    let names: Vec<&Value> = lists["tasks"].iter().map(|r| &r["name"]).collect();
    assert_eq!(names, [&json!("LOAD"), &json!("REPORT"), &json!("FIN")]);
    let load = &lists["tasks"][0];
    assert_eq!(load["after_root"], json!(true));
    assert_eq!(load["compute"], json!("serverless"));
    assert_eq!(load["initial_size"], json!("XSMALL"));
    assert_eq!(load["session"], json!("QUERY_TAG = 'x'"));
    assert_eq!(lists["tasks"][1]["after"], json!(["LOAD"]));
    assert_eq!(lists["tasks"][2]["role"], json!("finalizer"));
    assert!(form.notes.iter().any(|n| n.contains("raíz suspendida")));
    let p = task::plan(&f, Some(ROOT_ID), &values_of(&form))
        .await
        .unwrap();
    assert!(p.steps.is_empty(), "{:?}", p.steps);
    assert!(p.script().contains("No hay cambios"));
}

#[tokio::test]
async fn what_the_graph_has_is_kept_when_the_role_cant_list_it() {
    let mut f = server(r#"{"Predecessors":[]}"#, true);
    // The role sees neither the warehouse nor the integration the graph uses.
    for (k, s) in f.answers.iter_mut() {
        if k == "SHOW WAREHOUSES" {
            *s = set(&["name"], &[&["BIG_WH"]]);
        }
        if k == "SHOW NOTIFICATION INTEGRATIONS" {
            *s = set(&["name"], &[]);
        }
    }
    let form = task::form(&f, Some(ROOT_ID)).await.unwrap();
    assert!(form.choices["warehouse"].iter().any(|(v, _)| v == "WH"));
    let p = task::plan(&f, Some(ROOT_ID), &values_of(&form))
        .await
        .unwrap();
    assert!(p.steps.is_empty(), "{:?}", p.steps);
    // Another one it can't see is still refused.
    let mut v = values_of(&form);
    v.values.insert("warehouse".into(), json!("HIDDEN_WH"));
    assert!(err_of(task::plan(&f, Some(ROOT_ID), &v).await).contains("no está entre"));
}

#[tokio::test]
async fn a_change_suspends_the_started_root_and_resumes_it_last() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let p = edit_plan(&f, |v| {
        v.values.insert("comment".into(), json!("nuevo 'c'"));
    })
    .await
    .unwrap();
    assert_eq!(
        p.steps,
        [
            r#"ALTER TASK "DB"."ETL"."ROOT" SUSPEND"#,
            "ALTER TASK \"DB\".\"ETL\".\"ROOT\" SET\n  COMMENT = 'nuevo ''c'''",
            r#"ALTER TASK "DB"."ETL"."ROOT" RESUME"#,
        ]
    );
    assert_eq!(
        p.restore.as_deref(),
        Some(r#"ALTER TASK "DB"."ETL"."ROOT" RESUME"#)
    );
    assert!(p.script().contains("raíz suspendida"));
}

#[tokio::test]
async fn enabling_alone_is_a_resume_or_a_suspend() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let p = edit_plan(&f, |v| {
        v.values.insert("enabled".into(), json!(false));
    })
    .await
    .unwrap();
    assert_eq!(p.steps, [r#"ALTER TASK "DB"."ETL"."ROOT" SUSPEND"#]);
    assert!(p.restore.is_none());
    // Changed and left suspended: nothing to resume on a failure.
    let p = edit_plan(&f, |v| {
        v.values.insert("enabled".into(), json!(false));
        v.values.insert("comment".into(), json!(""));
    })
    .await
    .unwrap();
    assert_eq!(
        p.steps,
        [
            r#"ALTER TASK "DB"."ETL"."ROOT" SUSPEND"#,
            r#"ALTER TASK "DB"."ETL"."ROOT" UNSET COMMENT"#,
        ]
    );
    assert!(p.restore.is_none());
}

#[tokio::test]
async fn each_property_changes_in_place() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let p = edit_plan(&f, |v| {
        let x = &mut v.values;
        x.insert("compute".into(), json!("serverless"));
        x.insert("initial_size".into(), json!("SMALL"));
        x.insert("target_completion".into(), json!("15 minutes"));
        x.insert("schedule".into(), json!("interval"));
        x.insert("every".into(), json!(30));
        x.insert("unit".into(), json!("SECONDS"));
        x.insert("config".into(), json!(""));
        x.insert("overlap".into(), json!("ALLOW_ALL_OVERLAP"));
        x.insert("error_integration".into(), json!(""));
        x.insert("success_integration".into(), json!("NOTIFY"));
        x.insert("timeout_ms".into(), json!(""));
        x.insert("log_level".into(), json!("WARN"));
        x.insert("condition".into(), json!("SYSTEM$STREAM_HAS_DATA('S')"));
        x.insert("definition".into(), json!("CALL load_all();"));
        x.insert("execute_as".into(), json!("svc"));
        // Hidden with a schedule: left as the server has it (none).
        x.insert("min_trigger".into(), json!(60));
        x.insert("suspend_after".into(), json!(0));
        v.lists.insert(
            "session_params".into(),
            vec![map(json!({"name": "AUTOCOMMIT", "value": "true"}))],
        );
    })
    .await
    .unwrap();
    let r = r#""DB"."ETL"."ROOT""#;
    assert_eq!(
        p.steps,
        [
            format!("ALTER TASK {r} SUSPEND"),
            format!("ALTER TASK {r} UNSET CONFIG, ERROR_INTEGRATION, WAREHOUSE, USER_TASK_TIMEOUT_MS, TIMEZONE"),
            format!(
                "ALTER TASK {r} SET\n  USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE = 'SMALL'\n  SCHEDULE = '30 SECONDS'\n  OVERLAP_POLICY = ALLOW_ALL_OVERLAP\n  AUTOCOMMIT = TRUE\n  SUSPEND_TASK_AFTER_NUM_FAILURES = 0\n  SUCCESS_INTEGRATION = \"NOTIFY\"\n  LOG_LEVEL = 'WARN'\n  TARGET_COMPLETION_INTERVAL = '15 MINUTES'"
            ),
            format!("ALTER TASK {r} SET EXECUTE AS USER \"SVC\""),
            format!("ALTER TASK {r} MODIFY WHEN SYSTEM$STREAM_HAS_DATA('S')"),
            format!("ALTER TASK {r} MODIFY AS\nCALL load_all()"),
            format!("ALTER TASK {r} RESUME"),
        ]
    );
}

#[tokio::test]
async fn the_graph_is_reshaped_in_snowflake_order() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let p = edit_plan(&f, |v| {
        let rows = v.lists.get_mut("tasks").unwrap();
        // REPORT leaves the graph.
        rows.retain(|r| r["name"] != json!("REPORT"));
        // FIN becomes a child of the root.
        let fin = rows.iter_mut().find(|r| r["name"] == json!("FIN")).unwrap();
        fin.insert("role".into(), json!("child"));
        fin.insert("after_root".into(), json!(true));
        // A new one after LOAD, and LOAD's compute changes.
        rows.push(map(json!({
            "name": "new_one", "role": "child", "after_root": false, "after": ["LOAD"],
            "compute": "serverless", "definition": "SELECT 9", "enabled": true
        })));
        let load = rows
            .iter_mut()
            .find(|r| r["name"] == json!("LOAD"))
            .unwrap();
        load.insert("compute".into(), json!("warehouse"));
        load.insert("warehouse".into(), json!("BIG_WH"));
        load.insert("initial_size".into(), json!(""));
    })
    .await
    .unwrap();
    let t = |n: &str| format!(r#""DB"."ETL"."{n}""#);
    assert_eq!(
        p.steps,
        [
            format!("ALTER TASK {} SUSPEND", t("ROOT")),
            format!("ALTER TASK {} UNSET FINALIZE", t("FIN")),
            format!(
                "ALTER TASK {} UNSET USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE",
                t("LOAD")
            ),
            format!("ALTER TASK {} SET\n  WAREHOUSE = \"BIG_WH\"", t("LOAD")),
            format!("ALTER TASK {} ADD AFTER {}", t("FIN"), t("ROOT")),
            format!("DROP TASK {}", t("REPORT")),
            format!(
                "CREATE TASK {}\n  AFTER {}\nAS\nSELECT 9",
                t("NEW_ONE"),
                t("LOAD")
            ),
            format!("ALTER TASK {} RESUME", t("FIN")),
            format!("ALTER TASK {} RESUME", t("NEW_ONE")),
            format!("ALTER TASK {} RESUME", t("ROOT")),
        ]
    );
}

#[tokio::test]
async fn a_child_becomes_the_finalizer_after_the_old_one_goes() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let p = edit_plan(&f, |v| {
        let rows = v.lists.get_mut("tasks").unwrap();
        rows.retain(|r| r["name"] != json!("FIN"));
        let rep = rows
            .iter_mut()
            .find(|r| r["name"] == json!("REPORT"))
            .unwrap();
        rep.insert("role".into(), json!("finalizer"));
        rep.insert("enabled".into(), json!(true));
    })
    .await
    .unwrap();
    let t = |n: &str| format!(r#""DB"."ETL"."{n}""#);
    assert_eq!(
        p.steps,
        [
            format!("ALTER TASK {} SUSPEND", t("ROOT")),
            format!("DROP TASK {}", t("FIN")),
            format!("ALTER TASK {} REMOVE AFTER {}", t("REPORT"), t("LOAD")),
            format!("ALTER TASK {} SET FINALIZE = {}", t("REPORT"), t("ROOT")),
            format!("ALTER TASK {} RESUME", t("REPORT")),
            format!("ALTER TASK {} RESUME", t("ROOT")),
        ]
    );
}

#[tokio::test]
async fn a_childs_parameters_predecessors_and_state_change() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let p = edit_plan(&f, |v| {
        let load = row_mut(v, "LOAD");
        load.insert("session".into(), json!("QUERY_TAG = 'y'\nTIMEZONE = 'UTC'"));
        load.insert("enabled".into(), json!(false));
        let rep = row_mut(v, "REPORT");
        rep.insert("after_root".into(), json!(true));
        rep.insert("after".into(), json!([]));
        rep.insert("condition".into(), json!(""));
    })
    .await
    .unwrap();
    let t = |n: &str| format!(r#""DB"."ETL"."{n}""#);
    assert_eq!(
        p.steps,
        [
            format!("ALTER TASK {} SUSPEND", t("ROOT")),
            format!(
                "ALTER TASK {} SET\n  QUERY_TAG = 'y', TIMEZONE = 'UTC'",
                t("LOAD")
            ),
            format!("ALTER TASK {} REMOVE WHEN", t("REPORT")),
            format!("ALTER TASK {} ADD AFTER {}", t("REPORT"), t("ROOT")),
            format!("ALTER TASK {} REMOVE AFTER {}", t("REPORT"), t("LOAD")),
            format!("ALTER TASK {} SUSPEND", t("LOAD")),
            format!("ALTER TASK {} RESUME", t("ROOT")),
        ]
    );
}

#[tokio::test]
async fn an_edit_is_checked_against_the_server() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let e = err_of(
        edit_plan(&f, |v| {
            v.values.insert("name".into(), json!("ROOT2"));
        })
        .await,
    );
    assert!(e.contains("renombrar"), "{e}");
    let e = err_of(
        edit_plan(&f, |v| {
            row_mut(v, "LOAD").insert("name".into(), json!("LOADER"));
        })
        .await,
    );
    assert!(e.contains("renombrar"), "{e}");
    let e = err_of(
        edit_plan(&f, |v| {
            row_mut(v, "LOAD").insert("task".into(), json!("GONE"));
        })
        .await,
    );
    assert!(e.contains("ya no está"), "{e}");
    let e = err_of(
        edit_plan(&f, |v| {
            v.lists.get_mut("tasks").unwrap().push(map(json!({
                "name": "other", "role": "child", "after_root": true,
                "compute": "serverless", "definition": "SELECT 1"
            })));
        })
        .await,
    );
    assert!(e.contains("fuera de este grafo"), "{e}");
    let e = err_of(
        edit_plan(&f, |v| {
            v.values.insert("sf_id".into(), json!("another-id"));
        })
        .await,
    );
    assert!(e.contains("volvé a abrirlo"), "{e}");
    // A child's id is its graph's business.
    let e = err_of(task::form(&f, Some(r#""DB"."ETL"."LOAD""#)).await);
    assert!(e.contains("grafo de «ROOT»"), "{e}");
    let e = err_of(task::form(&f, Some("DB.ETL.ROOT; DROP TABLE x")).await);
    assert!(e.contains("no es un id de tarea"), "{e}");
    let e = err_of(task::form(&f, Some(r#""DB"."ETL"."MISSING""#)).await);
    assert!(e.contains("No existe la tarea"), "{e}");
}

#[tokio::test]
async fn another_roles_graph_is_read_only() {
    let f = server(r#"{"Predecessors":[]}"#, false);
    let form = task::form(&f, Some(ROOT_ID)).await.unwrap();
    assert!(form.read_only);
    assert!(form.notes[0].contains("rol propietario"));
    let v = values_of(&form);
    assert!(err_of(task::plan(&f, Some(ROOT_ID), &v).await).contains("rol propietario"));
    assert!(err_of(task::delete_plan(&f, ROOT_ID).await).contains("rol propietario"));
}

#[tokio::test]
async fn a_task_graph_objects_root_changes_without_suspending_and_commits() {
    let f = server(r#"{"Predecessors":[],"TaskGraph":"DB.ETL.PIPE"}"#, true);
    let form = task::form(&f, Some(ROOT_ID)).await.unwrap();
    assert_eq!(form.values["task_graph"], json!(r#""DB"."ETL"."PIPE""#));
    assert!(form
        .notes
        .iter()
        .any(|n| n.contains("se cambian en el grafo")));
    let p = edit_plan(&f, |v| {
        v.values.insert("comment".into(), json!("x"));
        // The object's: ignored here.
        v.values.insert("schedule".into(), json!("none"));
        v.values.insert("enabled".into(), json!(false));
    })
    .await
    .unwrap();
    assert_eq!(
        p.steps,
        [
            "ALTER TASK \"DB\".\"ETL\".\"ROOT\" SET\n  COMMENT = 'x'",
            r#"ALTER TASK GRAPH "DB"."ETL"."PIPE" COMMIT"#,
        ]
    );
    assert!(p.restore.is_none());
    let p = edit_plan(&f, |v| {
        v.values.insert("comment".into(), json!("x"));
        v.values.insert("commit".into(), json!(false));
    })
    .await
    .unwrap();
    assert_eq!(p.steps.len(), 1);
    let e = err_of(
        edit_plan(&f, |v| {
            v.lists
                .get_mut("tasks")
                .unwrap()
                .retain(|r| r["name"] != json!("FIN"));
        })
        .await,
    );
    assert!(e.contains("finalizador de un grafo de tareas"), "{e}");
    let e = err_of(task::delete_plan(&f, ROOT_ID).await);
    assert!(e.contains("Borrá antes el grafo"), "{e}");
}

#[tokio::test]
async fn deleting_a_graph_drops_its_tasks_root_last() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let p = task::delete_plan(&f, ROOT_ID).await.unwrap();
    let t = |n: &str| format!(r#""DB"."ETL"."{n}""#);
    assert_eq!(
        p.steps,
        [
            format!("ALTER TASK {} SUSPEND", t("ROOT")),
            format!("DROP TASK {}", t("FIN")),
            format!("DROP TASK {}", t("REPORT")),
            format!("DROP TASK {}", t("LOAD")),
            format!("DROP TASK {}", t("ROOT")),
        ]
    );
    assert!(p.script().contains("y las 3 tareas de su grafo"));
    // A standalone task goes alone, started or not.
    let p = task::delete_plan(&f, r#""DB"."ETL"."OTHER""#)
        .await
        .unwrap();
    assert_eq!(p.steps, [format!("DROP TASK {}", t("OTHER"))]);
}

#[tokio::test]
async fn a_save_runs_each_statement_and_resumes_the_root_when_one_fails() {
    let mut f = server(r#"{"Predecessors":[]}"#, true);
    f.fail = vec!["SET\n  COMMENT".into()];
    let form = task::form(&f, Some(ROOT_ID)).await.unwrap();
    let mut v = values_of(&form);
    v.values.insert("comment".into(), json!("nuevo"));
    let e = err_of(save(&f, "job", Some(ROOT_ID), &v).await);
    assert!(e.contains("instrucción 2 de 3"), "{e}");
    assert!(e.contains("la tarea raíz se reanudó"), "{e}");
    let seen = f.seen();
    let ddl: Vec<&String> = seen
        .iter()
        .filter(|s| s.starts_with("ALTER") || s.starts_with("CREATE") || s.starts_with("DROP"))
        .collect();
    assert_eq!(ddl.len(), 3, "{ddl:?}");
    assert!(ddl[0].ends_with("SUSPEND"));
    assert!(ddl[2].ends_with("RESUME"));
    // The script shows the same statements.
    let script = script(&f, "job", Some(ROOT_ID), &v).await.unwrap();
    assert!(script.contains("SET\n  COMMENT = 'nuevo';"), "{script}");
}

#[tokio::test]
async fn the_items_are_the_jobs_list() {
    let f = Fake::new(vec![
        ("SHOW TASKS IN ACCOUNT", tasks_set(r#"{"Predecessors":[]}"#)),
        ("CURRENT_ROLE", set(&["role"], &[&["ACCOUNTADMIN"]])),
    ]);
    let l = items(&f, "job").await.unwrap();
    let ids: Vec<&str> = l.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, [ROOT_ID, r#""DB"."ETL"."OTHER""#]);
    assert_eq!(l.items[0].enabled, Some(true));
    assert_eq!(l.items[0].description.as_deref(), Some("c"));
    // A task graph object's finalizer folds into its root too.
    let f = Fake::new(vec![(
        "SHOW TASKS IN ACCOUNT",
        tasks_set(r#"{"Predecessors":[],"TaskGraph":"DB.ETL.PIPE"}"#),
    )]);
    let l = items(&f, "job").await.unwrap();
    assert_eq!(l.items.len(), 2, "{:?}", l.items);
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

fn alert_row() -> AlertRow {
    AlertRow {
        database: "DB".into(),
        schema: "MON".into(),
        name: "HIGH_LOAD".into(),
        owner: Some("SYSADMIN".into()),
        comment: Some("old".into()),
        warehouse: Some("WH".into()),
        schedule: Some("5 MINUTE".into()),
        started: false,
        condition: Some("SELECT 1 FROM t WHERE x > 10".into()),
        action: Some("CALL notify()".into()),
        config: Some(r#"{"limit": 10}"#.into()),
        ..AlertRow::default()
    }
}

fn alert_values() -> FormValues {
    fv(
        json!({
            "name": "HIGH_LOAD", "schema": r#""DB"."MON""#, "comment": "old", "compute": "warehouse",
            "warehouse": "WH", "schedule": "interval", "every": 5, "source": "sql",
            "condition": "SELECT 1 FROM t WHERE x > 10", "action": "CALL notify()",
            "config": "{ \"limit\": 10 }", "enabled": false
        }),
        json!({}),
    )
}

#[test]
fn an_alert_is_created_with_its_condition_and_action() {
    let mut v = alert_values();
    v.values.insert("name".into(), json!("new_alert"));
    v.values.insert("schedule".into(), json!("cron"));
    v.values.insert("cron".into(), json!("0 9 * * MON-FRI"));
    v.values
        .insert("timezone".into(), json!("America/Argentina/Buenos_Aires"));
    v.values
        .insert("runbook".into(), json!("https://wiki/r?a='1'"));
    v.values.insert("suspend_after".into(), json!(3));
    v.values.insert("enabled".into(), json!(true));
    let s = alert::spec(&v, &choices(), None).unwrap();
    let p = alert::create_plan(&s);
    assert_eq!(
        p.steps,
        [
            "CREATE ALERT \"DB\".\"MON\".\"NEW_ALERT\"\n  SCHEDULE = 'USING CRON 0 9 * * MON-FRI America/Argentina/Buenos_Aires'\n  WAREHOUSE = \"WH\"\n  COMMENT = 'old'\n  CONFIG = '{ \"limit\": 10 }'\n  RUNBOOK = 'https://wiki/r?a=''1'''\n  SUSPEND_ALERT_AFTER_NUM_FAILURES = 3\n  IF (EXISTS (\nSELECT 1 FROM t WHERE x > 10\n  ))\n  THEN\nCALL notify()",
            "ALTER ALERT \"DB\".\"MON\".\"NEW_ALERT\" RESUME",
        ]
    );
    // From a template, serverless, on new data: the template's schedule.
    let mut v = alert_values();
    v.values.insert("source".into(), json!("template"));
    v.values
        .insert("template_id".into(), json!("tasks_error_rate"));
    v.values.insert(
        "template_params".into(),
        json!(r#"{"template_variables": {"ERROR_RATE_THRESHOLD": 0.4}}"#),
    );
    v.values.insert("compute".into(), json!("serverless"));
    v.values.insert("schedule".into(), json!("new_data"));
    v.values.insert("comment".into(), json!(""));
    let s = alert::spec(&v, &choices(), None).unwrap();
    assert_eq!(
        alert::create_plan(&s).steps,
        [r#"CREATE ALERT "DB"."MON"."HIGH_LOAD"
  FROM TEMPLATE TASKS_ERROR_RATE
  TEMPLATE_PARAMS = '{"template_variables": {"ERROR_RATE_THRESHOLD": 0.4}}'"#]
    );
    v.values.insert("template_id".into(), json!("x'; DROP"));
    assert!(alert::spec(&v, &choices(), None).is_err());
}

#[test]
fn an_alert_changes_each_part_in_place() {
    let a = alert_row();
    let cur = alert::current(&a);
    let v = alert_values();
    let same = alert::spec(&v, &choices(), Some(&a)).unwrap();
    assert!(alert::alter_plan(&cur, &same).steps.is_empty());
    let mut v = alert_values();
    let x = &mut v.values;
    x.insert("compute".into(), json!("serverless"));
    x.insert("every".into(), json!(15));
    x.insert("comment".into(), json!(""));
    x.insert("config".into(), json!("{\"limit\": 20}"));
    x.insert("runbook".into(), json!("ver wiki"));
    x.insert("suspend_after".into(), json!(5));
    x.insert("condition".into(), json!("SHOW TABLES LIKE 'x'"));
    x.insert(
        "action".into(),
        json!("BEGIN\n  CALL a();\n  CALL b();\nEND;"),
    );
    x.insert("enabled".into(), json!(true));
    let want = alert::spec(&v, &choices(), Some(&a)).unwrap();
    let id = r#""DB"."MON"."HIGH_LOAD""#;
    assert_eq!(
        alert::alter_plan(&cur, &want).steps,
        [
            format!("ALTER ALERT {id} UNSET WAREHOUSE, COMMENT"),
            format!("ALTER ALERT {id} SET\n  SCHEDULE = '15 MINUTE'\n  RUNBOOK = 'ver wiki'\n  CONFIG = '{{\"limit\": 20}}'\n  SUSPEND_ALERT_AFTER_NUM_FAILURES = 5"),
            format!("ALTER ALERT {id} MODIFY CONDITION EXISTS (\nSHOW TABLES LIKE 'x'\n)"),
            format!("ALTER ALERT {id} MODIFY ACTION\nBEGIN\n  CALL a();\n  CALL b();\nEND"),
            format!("ALTER ALERT {id} RESUME"),
        ]
    );
}

#[test]
fn an_alert_is_checked() {
    let a = alert_row();
    let mut v = alert_values();
    v.values.insert("schedule".into(), json!("new_data"));
    assert!(err_of(alert::spec(&v, &choices(), Some(&a))).contains("datos nuevos"));
    let mut v = alert_values();
    v.values.insert("condition".into(), json!("DELETE FROM t"));
    assert!(err_of(alert::spec(&v, &choices(), Some(&a))).contains("SELECT, SHOW o CALL"));
    let mut v = alert_values();
    v.values
        .insert("action".into(), json!("CALL a(); CALL b()"));
    assert!(err_of(alert::spec(&v, &choices(), Some(&a))).contains("una sola instrucción"));
    let mut v = alert_values();
    v.values.insert("name".into(), json!("OTHER"));
    assert!(err_of(alert::spec(&v, &choices(), Some(&a))).contains("renombrar"));
    let mut v = alert_values();
    v.values.insert("every".into(), json!(20000));
    assert!(alert::spec(&v, &choices(), Some(&a)).is_err());
    let mut v = alert_values();
    v.values.insert("runbook".into(), json!("x".repeat(2049)));
    assert!(err_of(alert::spec(&v, &choices(), Some(&a))).contains("2048"));
    assert_eq!(
        alert::drop_plan(&a).steps,
        [r#"DROP ALERT "DB"."MON"."HIGH_LOAD""#]
    );
}

#[tokio::test]
async fn the_alerts_form_reads_the_server() {
    let cols = [
        "database_name",
        "schema_name",
        "name",
        "owner",
        "owner_role_type",
        "comment",
        "warehouse",
        "schedule",
        "state",
        "condition",
        "action",
        "runbook",
        "config",
        "was_auto_suspended",
    ];
    let f = Fake::new(vec![
        (
            "SHOW ALERTS",
            set(
                &cols,
                &[&[
                    "DB",
                    "MON",
                    "HIGH_LOAD",
                    "SYSADMIN",
                    "ROLE",
                    "c",
                    "",
                    "",
                    "started",
                    "EXISTS (SELECT 1 FROM t)",
                    "CALL notify();",
                    "",
                    "",
                    "true",
                ]],
            ),
        ),
        ("SHOW WAREHOUSES", set(&["name"], &[&["WH"]])),
        ("IS_ROLE_IN_SESSION", set(&["r0"], &[&["true"]])),
    ]);
    let id = r#""DB"."MON"."HIGH_LOAD""#;
    let form = alert::form(&f, Some(id)).await.unwrap();
    assert_eq!(form.values["condition"], json!("SELECT 1 FROM t"));
    assert_eq!(form.values["action"], json!("CALL notify()"));
    assert_eq!(form.values["schedule"], json!("new_data"));
    assert_eq!(form.values["compute"], json!("serverless"));
    assert_eq!(form.values["auto_suspended"], json!("Sí"));
    assert!(form.notes.iter().any(|n| n.contains("no permite pasar")));
    let p = alert::plan(&f, Some(id), &values_of(&form)).await.unwrap();
    assert!(p.steps.is_empty(), "{:?}", p.steps);
    let l = items(&f, "alert").await.unwrap();
    assert_eq!(l.items[0].id, id);
    assert_eq!(l.items[0].enabled, Some(true));
}

// ---------------------------------------------------------------------------
// Task graph objects
// ---------------------------------------------------------------------------

fn graph_read() -> graph::Read {
    graph::Read {
        graph: GraphRow {
            database: "DB".into(),
            schema: "ETL".into(),
            name: "PIPE".into(),
            owner: Some("SYSADMIN".into()),
            comment: Some("c".into()),
            schedule: Some("USING CRON 0 * * * * UTC".into()),
            started: true,
            defaults: Some(
                r#"{"WAREHOUSE": "WH", "LOG_LEVEL": "INFO", "EXECUTE AS USER": "SVC"}"#.into(),
            ),
            pending: Some("false".into()),
            overlap: Some("NO_OVERLAP".into()),
            ..GraphRow::default()
        },
        root: Some("ROOT".into()),
        finalizer: Some("FIN".into()),
        tasks: task::shown_rows(&tasks_set(
            r#"{"Predecessors":[],"TaskGraph":"DB.ETL.PIPE"}"#,
        )),
    }
}

fn graph_values() -> FormValues {
    fv(
        json!({
            "name": "PIPE", "schema": r#""DB"."ETL""#, "comment": "c", "enabled": true, "commit": true,
            "schedule": "cron", "cron": "0 * * * *", "timezone": "UTC",
            "root": r#""DB"."ETL"."ROOT""#, "finalizer": r#""DB"."ETL"."FIN""#,
            "overlap": "NO_OVERLAP", "d_warehouse": "WH", "d_log_level": "INFO", "d_execute_as": "SVC"
        }),
        json!({}),
    )
}

#[test]
fn a_task_graph_is_created_committed_and_resumed() {
    let mut v = graph_values();
    v.values.insert("name".into(), json!("pipe2"));
    v.values.insert("auto_retry".into(), json!(2));
    v.values.insert("d_timeout_ms".into(), json!(1000));
    v.values.insert("condition".into(), json!("TRUE"));
    let s = graph::spec(&v, &choices(), None).unwrap();
    let p = graph::create_plan(&s);
    let id = r#""DB"."ETL"."PIPE2""#;
    assert_eq!(
        p.steps,
        [
            format!("CREATE TASK GRAPH {id}\n  SCHEDULE = 'USING CRON 0 * * * * UTC'\n  ROOTS = ( \"DB\".\"ETL\".\"ROOT\" )\n  FINALIZER = \"DB\".\"ETL\".\"FIN\"\n  TASK_AUTO_RETRY_ATTEMPTS = 2\n  COMMENT = 'c'\n  TASK_DEFAULTS ( WAREHOUSE = \"WH\", USER_TASK_TIMEOUT_MS = 1000, LOG_LEVEL = 'INFO', EXECUTE AS USER \"SVC\" )\n  WHEN TRUE"),
            format!("ALTER TASK GRAPH {id} COMMIT"),
            format!("ALTER TASK GRAPH {id} RESUME"),
        ]
    );
}

#[test]
fn a_task_graph_changes_in_place_and_commits() {
    let rd = graph_read();
    let cur = graph::current(&rd);
    assert_eq!(cur.defaults.execute_as.as_deref(), Some("SVC"));
    let same = graph::spec(&graph_values(), &choices(), Some(&rd)).unwrap();
    assert!(graph::alter_plan(&cur, &same, false).steps.is_empty());
    // Pending changes from before are committed when asked.
    assert_eq!(
        graph::alter_plan(&cur, &same, true).steps,
        [r#"ALTER TASK GRAPH "DB"."ETL"."PIPE" COMMIT"#]
    );
    let mut v = graph_values();
    let x = &mut v.values;
    x.insert("schedule".into(), json!("interval"));
    x.insert("every".into(), json!(2));
    x.insert("unit".into(), json!("HOURS"));
    x.insert("root".into(), json!(r#""DB"."ETL"."OTHER""#));
    x.insert("finalizer".into(), json!(""));
    x.insert("d_log_level".into(), json!(""));
    x.insert("d_warehouse".into(), json!("BIG_WH"));
    x.insert("condition".into(), json!("SYSTEM$STREAM_HAS_DATA('S')"));
    x.insert("enabled".into(), json!(false));
    let want = graph::spec(&v, &choices(), Some(&rd)).unwrap();
    let id = r#""DB"."ETL"."PIPE""#;
    assert_eq!(
        graph::alter_plan(&cur, &want, false).steps,
        [
            format!("ALTER TASK GRAPH {id} UNSET FINALIZER"),
            format!("ALTER TASK GRAPH {id} SET\n  SCHEDULE = '2 HOURS',\n  ROOTS = ( \"DB\".\"ETL\".\"OTHER\" )"),
            format!("ALTER TASK GRAPH {id} UNSET TASK_DEFAULTS ( LOG_LEVEL )"),
            format!("ALTER TASK GRAPH {id} SET TASK_DEFAULTS ( WAREHOUSE = \"BIG_WH\" )"),
            format!("ALTER TASK GRAPH {id} MODIFY WHEN SYSTEM$STREAM_HAS_DATA('S')"),
            format!("ALTER TASK GRAPH {id} COMMIT"),
            format!("ALTER TASK GRAPH {id} SUSPEND"),
        ]
    );
    assert_eq!(
        graph::drop_plan(&rd.graph).steps,
        [
            format!("ALTER TASK GRAPH {id} SUSPEND"),
            format!("DROP TASK GRAPH {id}"),
        ]
    );
}

#[test]
fn a_task_graph_is_checked() {
    let rd = graph_read();
    let mut v = graph_values();
    v.values
        .insert("root".into(), json!(r#""DB"."OTHER"."ROOT""#));
    assert!(err_of(graph::spec(&v, &choices(), Some(&rd))).contains("esquema del grafo"));
    let mut v = graph_values();
    v.values
        .insert("finalizer".into(), json!(r#""DB"."ETL"."ROOT""#));
    assert!(err_of(graph::spec(&v, &choices(), Some(&rd))).contains("misma tarea"));
    let mut v = graph_values();
    v.values
        .insert("root".into(), json!(r#""DB"."ETL"."NOPE""#));
    assert!(err_of(graph::spec(&v, &choices(), Some(&rd))).contains("no existe la tarea"));
    let mut v = graph_values();
    v.values.insert("schedule".into(), json!("none"));
    assert!(err_of(graph::spec(&v, &choices(), Some(&rd))).contains("programación o una condición"));
    let mut v = graph_values();
    v.values.insert("d_initial_size".into(), json!("HUGE"));
    assert!(graph::spec(&v, &choices(), Some(&rd)).is_err());
    let mut v = graph_values();
    v.values.insert("name".into(), json!("OTHER"));
    assert!(err_of(graph::spec(&v, &choices(), Some(&rd))).contains("renombrar"));
}

#[tokio::test]
async fn a_missing_task_graph_preview_is_an_empty_list_with_a_note() {
    let mut f = Fake::new(vec![]);
    f.fail = vec!["SHOW TASK GRAPHS".into()];
    let l = items(&f, "task_graph").await.unwrap();
    assert!(l.items.is_empty());
    assert!(l.notes[0].contains("vista previa"));
}

// ---------------------------------------------------------------------------
// Logs, start at a task, history
// ---------------------------------------------------------------------------

fn graph_of(root_rel: &str) -> task::Graph {
    let tasks = task::shown_rows(&tasks_set(root_rel));
    let members = task::members_of(&tasks, 0);
    task::Graph {
        root: tasks[0].clone(),
        root_params: Default::default(),
        members: members
            .iter()
            .map(|&i| (tasks[i].clone(), Default::default()))
            .collect(),
        schema_tasks: tasks.iter().map(|t| t.name.clone()).collect(),
        orphan: false,
    }
}

#[test]
fn a_graph_starts_at_its_root_or_retries_where_it_failed() {
    let g = graph_of(r#"{"Predecessors":[]}"#);
    assert_eq!(
        logs::run_from_sql(&g, "ROOT", None).unwrap(),
        format!("EXECUTE TASK {ROOT_ID}")
    );
    let history = set(
        &[
            "name",
            "state",
            "graph_run_group_id",
            "attempt_number",
            "scheduled_ms",
        ],
        &[
            &["ROOT", "SUCCEEDED", "g1", "1", "100"],
            &["LOAD", "SUCCEEDED", "g1", "1", "110"],
            &["REPORT", "FAILED", "g1", "1", "120"],
            &["ROOT", "SUCCEEDED", "g0", "1", "50"],
            &["REPORT", "SUCCEEDED", "g0", "1", "60"],
        ],
    );
    let last = logs::last_run(&history, "ROOT");
    assert_eq!(last.get("REPORT").map(String::as_str), Some("FAILED"));
    assert_eq!(
        logs::run_from_sql(&g, "REPORT", Some(&last)).unwrap(),
        format!("EXECUTE TASK {ROOT_ID} RETRY LAST")
    );
    let e = err_of(logs::run_from_sql(&g, "LOAD", Some(&last)));
    assert!(
        e.contains("solo por su raíz") && e.contains("Correcta"),
        "{e}"
    );
    let e = err_of(logs::run_from_sql(&g, "FIN", Some(&last)));
    assert!(e.contains("no llegó"), "{e}");
    assert!(err_of(logs::run_from_sql(&g, "NOPE", Some(&last))).contains("no tiene una tarea"));
    // A retry of the retry: the latest attempt counts.
    let history = set(
        &[
            "name",
            "state",
            "graph_run_group_id",
            "attempt_number",
            "scheduled_ms",
        ],
        &[
            &["ROOT", "SUCCEEDED", "g1", "1", "100"],
            &["REPORT", "FAILED", "g1", "1", "120"],
            &["REPORT", "EXECUTING", "g1", "2", "130"],
        ],
    );
    let last = logs::last_run(&history, "ROOT");
    assert!(err_of(logs::run_from_sql(&g, "REPORT", Some(&last))).contains("se está ejecutando"));
    // A task graph object runs by the object.
    let g = graph_of(r#"{"Predecessors":[],"TaskGraph":"DB.ETL.PIPE"}"#);
    assert_eq!(
        logs::run_from_sql(&g, "ROOT", None).unwrap(),
        r#"EXECUTE TASK GRAPH "DB"."ETL"."PIPE""#
    );
}

#[tokio::test]
async fn starting_at_a_task_reads_the_history_only_with_the_warehouse_running() {
    let mut f = server(r#"{"Predecessors":[]}"#, true);
    f.answers.insert(
        0,
        (
            "SHOW WAREHOUSES LIKE".into(),
            set(&["name", "state"], &[&["WH", "SUSPENDED"]]),
        ),
    );
    let e = err_of(run_from(&f, ROOT_ID, "REPORT").await);
    assert!(e.contains("suspendido"), "{e}");
    assert!(!f
        .seen()
        .iter()
        .any(|s| s.contains("TASK_HISTORY") || s.contains("RESUME")));
    // The root needs no history.
    run_from(&f, ROOT_ID, "ROOT").await.unwrap();
    assert_eq!(f.seen().last().unwrap(), &format!("EXECUTE TASK {ROOT_ID}"));
}

#[tokio::test]
async fn the_log_never_wakes_a_warehouse_and_the_history_is_not_cleared() {
    let f = Fake::new(vec![(
        "SHOW WAREHOUSES",
        set(&["name", "state"], &[&["WH", "SUSPENDED"]]),
    )]);
    let e = err_of(logs(&f).await);
    assert!(e.contains("no lo reanuda"), "{e}");
    assert!(!f.seen().iter().any(|s| s.contains("HISTORY")));
    let f = Fake::new(vec![
        (
            "SHOW WAREHOUSES",
            set(&["name", "state"], &[&["WH", "STARTED"]]),
        ),
        (
            "ALERT_HISTORY",
            set(
                &[
                    "scheduled",
                    "completed",
                    "database_name",
                    "schema_name",
                    "name",
                    "state",
                    "scheduled_from",
                    "sql_error_code",
                    "sql_error_message",
                ],
                &[&[
                    "2026-11-12 10:00:00 +00:00",
                    "2026-11-12 10:00:02 +00:00",
                    "DB",
                    "MON",
                    "HIGH_LOAD",
                    "ACTION_FAILED",
                    "SCHEDULE",
                    "100",
                    "boom",
                ]],
            ),
        ),
    ]);
    let mut out = logs(&f).await.unwrap();
    assert_eq!(out.len(), 2);
    let alerts = out.remove(0);
    assert_eq!(alerts.title, "Ejecuciones de alertas");
    assert_eq!(
        alerts.rows[0],
        vec![
            json!("2026-11-12 10:00:00 +00:00"),
            json!("2026-11-12 10:00:02 +00:00"),
            json!("DB.MON.HIGH_LOAD"),
            json!("Falló la acción"),
            json!("programada"),
            json!("boom (100)"),
        ]
    );
    assert!(f.seen().iter().any(|s| s.contains("ERROR_ONLY => TRUE")));
    let e = clear_history(&f, ROOT_ID).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)));
    assert!(clear_history(&f, "nope").await.is_err());
}

// ---------------------------------------------------------------------------
// Flags, actions and what each object allows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_kind_says_what_it_does_besides_editing() {
    let f = Fake::new(vec![("CURRENT_ROLE", set(&["role"], &[&["SYSADMIN"]]))]);
    let i = info(&f).await.unwrap();
    let (job, alert, graph) = (&i.kinds[0], &i.kinds[1], &i.kinds[2]);
    // A graph starts at its root or retries where it failed; Snowflake
    // keeps its history. "Ejecutar ahora" is the jobs list's.
    assert!(job.run_from_step && !job.clear_history);
    assert!(job.actions.is_empty());
    for k in [alert, graph] {
        assert!(!k.run_from_step && !k.clear_history, "{}", k.key);
        assert_eq!(
            k.actions,
            [SchedulerAction::new("execute", "Ejecutar ahora")],
            "{}",
            k.key
        );
    }
    // Without task graph objects, the alerts keep theirs.
    let mut f = Fake::new(vec![]);
    f.errors = vec![(
        "SHOW TASK GRAPHS".into(),
        "Unsupported feature 'TASK GRAPH'. (SQLSTATE 0A000)".into(),
    )];
    let i = info(&f).await.unwrap();
    assert_eq!(i.kinds.len(), 2);
    assert_eq!(i.kinds[1].actions.len(), 1);
}

const ALERT_COLS: [&str; 14] = [
    "database_name",
    "schema_name",
    "name",
    "owner",
    "owner_role_type",
    "comment",
    "warehouse",
    "schedule",
    "state",
    "condition",
    "action",
    "runbook",
    "config",
    "was_auto_suspended",
];

/// An alert of `MON` owned by `owner` (`APPLICATION` for an app's).
fn alert_of<'a>(name: &'a str, owner: &'a str, owner_type: &'a str) -> Vec<&'a str> {
    vec![
        "DB",
        "MON",
        name,
        owner,
        owner_type,
        "",
        "WH",
        "5 MINUTE",
        "started",
        "SELECT 1",
        "CALL notify()",
        "",
        "",
        "false",
    ]
}

fn graphs_set(rows: &[(&str, &str)]) -> Set {
    let rows: Vec<Vec<&str>> = rows
        .iter()
        .map(|(name, owner)| vec!["DB", "ETL", *name, *owner, "started"])
        .collect();
    let refs: Vec<&[&str]> = rows.iter().map(Vec::as_slice).collect();
    set(
        &["database_name", "schema_name", "name", "owner", "state"],
        &refs,
    )
}

#[tokio::test]
async fn alerts_and_task_graphs_run_now_by_their_action() {
    let f = Fake::new(vec![
        (
            "SHOW ALERTS",
            set(&ALERT_COLS, &[&alert_of("HIGH_LOAD", "OTHER_ROLE", "ROLE")]),
        ),
        ("SHOW TASK GRAPHS", graphs_set(&[("PIPE", "OTHER_ROLE")])),
        (
            "SHOW TASKS IN SCHEMA",
            tasks_set(r#"{"Predecessors":[],"TaskGraph":"DB.ETL.PIPE"}"#),
        ),
        ("IS_ROLE_IN_SESSION", set(&["r0"], &[&["false"]])),
    ]);
    let alert = r#""DB"."MON"."HIGH_LOAD""#;
    let s = action_script(&f, "alert", alert, "execute").await.unwrap();
    assert!(
        s.starts_with("-- Ejecuta ahora la alerta DB.MON.HIGH_LOAD"),
        "{s}"
    );
    assert!(s.ends_with(&format!("\nEXECUTE ALERT {alert};\n")), "{s}");
    // Not only the owner's: another role's alert runs if Snowflake lets it.
    action(&f, "alert", alert, " execute ").await.unwrap();
    assert_eq!(f.seen().last().unwrap(), &format!("EXECUTE ALERT {alert}"));
    let pipe = r#""DB"."ETL"."PIPE""#;
    let s = action_script(&f, "task_graph", pipe, "execute")
        .await
        .unwrap();
    assert!(
        s.contains(&format!("\nEXECUTE TASK GRAPH {pipe};\n")),
        "{s}"
    );
    action(&f, "task_graph", pipe, "execute").await.unwrap();
    assert_eq!(
        f.seen().last().unwrap(),
        &format!("EXECUTE TASK GRAPH {pipe}")
    );
    // The same statement as "Ejecutar ahora" on the graph's root job.
    run_now(&f, "DB", "ETL", "ROOT").await.unwrap();
    assert_eq!(
        f.seen().last().unwrap(),
        &format!("EXECUTE TASK GRAPH {pipe}")
    );

    // What isn't an action, or an object that isn't there, runs nothing.
    let f = Fake::new(vec![("SHOW ALERTS", set(&ALERT_COLS, &[]))]);
    for (kind, id, act, says) in [
        ("job", ROOT_ID, "execute", "no tiene la acción"),
        ("alert", alert, "run", "no tiene la acción"),
        (
            "alert",
            alert,
            "execute; DROP ALERT x",
            "no tiene la acción",
        ),
        ("alert", "nope", "execute", "no es un id de alerta"),
        ("alert", alert, "execute", "No existe la alerta"),
        ("operator", alert, "execute", "no es un tipo"),
    ] {
        let e = err_of(action_script(&f, kind, id, act).await);
        assert!(e.contains(says), "{kind} {act}: {e}");
        let e = err_of(action(&f, kind, id, act).await);
        assert!(e.contains(says), "{kind} {act}: {e}");
    }
    assert!(
        !f.seen().iter().any(|s| s.starts_with("EXECUTE")),
        "{:?}",
        f.seen()
    );
}

#[tokio::test]
async fn items_say_which_objects_the_role_may_change() {
    // Tasks: another role's graph is shown but not changed nor dropped.
    let shown = tasks_set(r#"{"Predecessors":[]}"#);
    let mut f = Fake::new(vec![
        ("SHOW TASKS IN ACCOUNT", shown.clone()),
        ("IS_ROLE_IN_SESSION", set(&["r0"], &[&["false"]])),
    ]);
    let l = items(&f, "job").await.unwrap();
    assert_eq!(l.items.len(), 2);
    for i in &l.items {
        assert_eq!(
            (i.can_edit, i.can_delete),
            (Some(false), Some(false)),
            "{}",
            i.id
        );
        assert_eq!(i.actions, None);
    }
    assert!(f
        .seen()
        .iter()
        .any(|s| s.contains("IS_ROLE_IN_SESSION('SYSADMIN')")));
    f.answers[1].1 = set(&["r0"], &[&["true"]]);
    let l = items(&f, "job").await.unwrap();
    assert!(l
        .items
        .iter()
        .all(|i| i.can_edit.is_none() && i.can_delete.is_none() && !i.system));
    // Roots and standalone tasks start at a step as the kind says.
    assert!(l
        .items
        .iter()
        .all(|i| i.run_from_step.is_none() && i.clear_history.is_none()));
    // A task graph object's root changes in place but goes only with its
    // graph.
    f.answers[0].1 = tasks_set(r#"{"Predecessors":[],"TaskGraph":"DB.ETL.PIPE"}"#);
    let l = items(&f, "job").await.unwrap();
    assert_eq!(l.items[0].id, ROOT_ID);
    assert_eq!(
        (l.items[0].can_edit, l.items[0].can_delete),
        (None, Some(false))
    );
    assert_eq!(l.items[1].can_delete, None);
    // A native application's task, and a child whose root the role doesn't
    // see, can't be changed nor dropped, whoever the session is.
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in &shown.rows {
        let name = row[2].clone().unwrap_or_default();
        if name == "ROOT" {
            continue;
        }
        let mut r: Vec<String> = row.iter().map(|c| c.clone().unwrap_or_default()).collect();
        if name == "OTHER" {
            r[4] = "THE_APP".into();
            r[5] = "APPLICATION".into();
        }
        if name == "LOAD" {
            r[4] = "LOST_ROLE".into();
        }
        rows.push(r);
    }
    let refs: Vec<Vec<&str>> = rows
        .iter()
        .map(|r| r.iter().map(String::as_str).collect())
        .collect();
    let refs: Vec<&[&str]> = refs.iter().map(Vec::as_slice).collect();
    let f = Fake::new(vec![
        ("SHOW TASKS IN ACCOUNT", set(&TASK_COLS, &refs)),
        ("IS_ROLE_IN_SESSION", set(&["r0"], &[&["true"]])),
    ]);
    let l = items(&f, "job").await.unwrap();
    let load = l.items.iter().find(|i| i.name == "LOAD").unwrap();
    assert_eq!((load.can_edit, load.can_delete), (Some(false), Some(false)));
    let other = l.items.iter().find(|i| i.name == "OTHER").unwrap();
    assert_eq!(
        (other.can_edit, other.can_delete),
        (Some(false), Some(false))
    );
    assert!(other.system);
    // The orphan child doesn't start at a step (it runs only with its
    // graph); a standalone task is as its kind says.
    assert_eq!(load.run_from_step, Some(false));
    assert_eq!(other.run_from_step, None);
    // Their owners aren't asked about: the app isn't a role, and the
    // orphan is decided already.
    assert!(
        !f.seen()
            .iter()
            .any(|s| s.contains("'THE_APP'") || s.contains("'LOST_ROLE'")),
        "{:?}",
        f.seen()
    );

    // Alerts: the roles are asked once, in name order (r0, r1), a native
    // application's owner left out.
    let f = Fake::new(vec![
        (
            "SHOW ALERTS",
            set(
                &ALERT_COLS,
                &[
                    &alert_of("MINE", "ADMIN_ROLE", "ROLE"),
                    &alert_of("THEIRS", "OTHER_ROLE", "ROLE"),
                    &alert_of("APPS", "APP", "APPLICATION"),
                ],
            ),
        ),
        (
            "IS_ROLE_IN_SESSION",
            set(&["r0", "r1"], &[&["true", "false"]]),
        ),
    ]);
    let l = items(&f, "alert").await.unwrap();
    let by = |n: &str| l.items.iter().find(|i| i.name == n).unwrap().clone();
    assert_eq!((by("MINE").can_edit, by("MINE").can_delete), (None, None));
    assert_eq!(
        (by("THEIRS").can_edit, by("THEIRS").can_delete),
        (Some(false), Some(false))
    );
    assert_eq!(
        (by("APPS").can_edit, by("APPS").can_delete),
        (Some(false), Some(false))
    );
    assert!(by("APPS").system);
    let asked: Vec<String> = f
        .seen()
        .into_iter()
        .filter(|s| s.contains("IS_ROLE_IN_SESSION"))
        .collect();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].contains("IS_ROLE_IN_SESSION('ADMIN_ROLE') AS r0"));
    assert!(asked[0].contains("IS_ROLE_IN_SESSION('OTHER_ROLE') AS r1"));
    assert!(!asked[0].contains("'APP'"), "{asked:?}");

    // Task graph objects.
    let f = Fake::new(vec![
        (
            "SHOW TASK GRAPHS",
            graphs_set(&[("A", "ADMIN_ROLE"), ("B", "OTHER_ROLE")]),
        ),
        (
            "IS_ROLE_IN_SESSION",
            set(&["r0", "r1"], &[&["true", "false"]]),
        ),
    ]);
    let l = items(&f, "task_graph").await.unwrap();
    assert_eq!((l.items[0].can_edit, l.items[0].can_delete), (None, None));
    assert_eq!(
        (l.items[1].can_edit, l.items[1].can_delete),
        (Some(false), Some(false))
    );
    // A session that can't tell its roles leaves it to Snowflake.
    let mut f = Fake::new(vec![(
        "SHOW TASK GRAPHS",
        graphs_set(&[("B", "OTHER_ROLE")]),
    )]);
    f.fail = vec!["IS_ROLE_IN_SESSION".into()];
    let l = items(&f, "task_graph").await.unwrap();
    assert_eq!(l.items[0].can_edit, None);
}

#[tokio::test]
async fn the_minimum_trigger_interval_shows_only_for_a_triggered_task() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let form = task::form(&f, Some(ROOT_ID)).await.unwrap();
    let field = form
        .sections
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|f| f.key == "min_trigger")
        .unwrap();
    assert_eq!(
        field.when,
        Some(("schedule".to_string(), vec!["none".to_string()]))
    );
}

#[tokio::test]
async fn a_hidden_minimum_trigger_interval_is_left_alone() {
    const KEY: &str = "USER_TASK_MINIMUM_TRIGGER_INTERVAL_IN_SECONDS";
    // A new scheduled task: the hidden value is neither checked nor sent.
    let c = choices();
    for hidden in [json!(5), json!(60), json!("x")] {
        let mut v = new_graph_values();
        v.values.insert("min_trigger".into(), hidden.clone());
        let s = task::spec(&v, &c, None).unwrap();
        assert_eq!(s.root.min_trigger, None, "{hidden}");
        let p = task::create_plan(&s).unwrap();
        assert!(!p.steps[0].contains(KEY), "{}", p.steps[0]);
    }
    // A triggered one (no schedule): checked and sent.
    let mut v = new_graph_values();
    v.values.insert("schedule".into(), json!("none"));
    v.values.insert("min_trigger".into(), json!(5));
    assert!(err_of(task::spec(&v, &c, None)).contains("Intervalo mínimo"));
    v.values.insert("min_trigger".into(), json!(60));
    let p = task::create_plan(&task::spec(&v, &c, None).unwrap()).unwrap();
    assert!(
        p.steps[0].contains(&format!("{KEY} = 60")),
        "{}",
        p.steps[0]
    );

    // An edit of a scheduled root that has its own value: hidden, kept
    // whatever the form sends back for it (no UNSET, no range error).
    let mut f = server(r#"{"Predecessors":[]}"#, true);
    for (k, s) in f.answers.iter_mut() {
        if k == r#"SHOW PARAMETERS IN TASK "DB"."ETL"."ROOT""# {
            *s = params_set(&[
                ("USER_TASK_TIMEOUT_MS", "60000", "NUMBER"),
                ("TIMEZONE", "UTC", "STRING"),
                (KEY, "120", "NUMBER"),
            ]);
        }
    }
    let form = task::form(&f, Some(ROOT_ID)).await.unwrap();
    assert_eq!(form.values["schedule"], json!("cron"));
    assert_eq!(form.values["min_trigger"], json!(120));
    for hidden in [json!(""), json!(5), json!(60)] {
        let mut v = values_of(&form);
        v.values.insert("min_trigger".into(), hidden.clone());
        let p = task::plan(&f, Some(ROOT_ID), &v).await.unwrap();
        assert!(p.steps.is_empty(), "{hidden}: {:?}", p.steps);
    }
    // Made a triggered task, the value is the form's again.
    let mut v = values_of(&form);
    v.values.insert("schedule".into(), json!("none"));
    v.values.insert("min_trigger".into(), json!(60));
    let p = task::plan(&f, Some(ROOT_ID), &v).await.unwrap();
    let all = p.steps.join("\n");
    assert!(all.contains("UNSET SCHEDULE"), "{all}");
    assert!(all.contains(&format!("{KEY} = 60")), "{all}");
}

// ---------------------------------------------------------------------------
// Warnings: the "⚠" lines the app lists before a script runs
// ---------------------------------------------------------------------------

/// A script's warnings, as the app reads them.
fn warned(script: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|l| l.strip_prefix("-- ⚠ "))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn a_task_change_warns_only_about_what_it_risks() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    // Nothing to save, or one statement: nothing to accept.
    let p = edit_plan(&f, |_| {}).await.unwrap();
    assert!(warned(&p.script()).is_empty(), "{}", p.script());
    let p = edit_plan(&f, |v| {
        v.values.insert("enabled".into(), json!(false));
    })
    .await
    .unwrap();
    assert!(warned(&p.script()).is_empty(), "{}", p.script());
    // Several statements around the started root, suspended for them.
    let p = edit_plan(&f, |v| {
        v.values.insert("comment".into(), json!("x"));
    })
    .await
    .unwrap();
    assert_eq!(warned(&p.script()), [NOT_ATOMIC_ROOT], "{}", p.script());
    // Several, the root left suspended as asked: nothing to resume.
    let p = edit_plan(&f, |v| {
        v.values.insert("enabled".into(), json!(false));
        v.values.insert("comment".into(), json!(""));
    })
    .await
    .unwrap();
    assert_eq!(warned(&p.script()), [NOT_ATOMIC]);
    // A task taken out of the graph is dropped.
    let p = edit_plan(&f, |v| {
        v.lists
            .get_mut("tasks")
            .unwrap()
            .retain(|r| r["name"] != json!("REPORT"));
    })
    .await
    .unwrap();
    assert!(p.steps.iter().any(|s| s.starts_with("DROP TASK")));
    assert_eq!(
        warned(&p.script()),
        [
            "Las tareas que se quitaron del grafo se borran: REPORT.",
            NOT_ATOMIC_ROOT
        ]
    );
    // A new graph is several statements too.
    let s = task::spec(&new_graph_values(), &choices(), None).unwrap();
    assert_eq!(
        warned(&task::create_plan(&s).unwrap().script()),
        [NOT_ATOMIC]
    );
}

#[tokio::test]
async fn deleting_a_graph_warns_that_its_tasks_go_with_it() {
    let f = server(r#"{"Predecessors":[]}"#, true);
    let p = task::delete_plan(&f, ROOT_ID).await.unwrap();
    assert_eq!(warned(&p.script()), [GRAPH_GOES, NOT_ATOMIC]);
    assert!(
        p.script()
            .starts_with("-- Borra la tarea DB.ETL.ROOT de Snowflake y las 3 tareas de su grafo\n"),
        "{}",
        p.script()
    );
    // A root with one other task: the same sentence, and the header says
    // it in the singular.
    let mut g = task::read(&f, ROOT_ID).await.unwrap();
    g.members.truncate(1);
    let p = task::drop_plan(&g).unwrap();
    assert_eq!(warned(&p.script()), [GRAPH_GOES, NOT_ATOMIC]);
    assert!(
        p.script().starts_with(
            "-- Borra la tarea DB.ETL.ROOT de Snowflake y la otra tarea de su grafo\n"
        ),
        "{}",
        p.script()
    );
    // A standalone task: one DROP, nothing else goes.
    let p = task::delete_plan(&f, r#""DB"."ETL"."OTHER""#)
        .await
        .unwrap();
    assert!(warned(&p.script()).is_empty(), "{}", p.script());
}

#[tokio::test]
async fn a_commit_warns_when_it_publishes_changes_staged_before() {
    // A task graph object's own form.
    let rd = graph_read();
    let cur = graph::current(&rd);
    let same = graph::spec(&graph_values(), &choices(), Some(&rd)).unwrap();
    assert_eq!(
        warned(&graph::alter_plan(&cur, &same, true).script()),
        [PENDING_COMMIT]
    );
    let mut v = graph_values();
    v.values.insert("comment".into(), json!("x"));
    let want = graph::spec(&v, &choices(), Some(&rd)).unwrap();
    assert_eq!(
        warned(&graph::alter_plan(&cur, &want, false).script()),
        [NOT_ATOMIC]
    );
    // Not committed: the staged changes stay staged.
    v.values.insert("commit".into(), json!(false));
    let want = graph::spec(&v, &choices(), Some(&rd)).unwrap();
    let p = graph::alter_plan(&cur, &want, true);
    assert_eq!(p.steps.len(), 1);
    assert!(warned(&p.script()).is_empty(), "{}", p.script());
    // Dropping a started one suspends it first.
    assert_eq!(warned(&graph::drop_plan(&rd.graph).script()), [NOT_ATOMIC]);

    // Its root's graph, changed from the task's form: Snowflake says
    // whether the object has changes staged.
    let mut f = server(r#"{"Predecessors":[],"TaskGraph":"DB.ETL.PIPE"}"#, true);
    let comment = |v: &mut FormValues| {
        v.values.insert("comment".into(), json!("x"));
    };
    let staged = |p: &Plan| {
        (
            p.header.iter().any(|h| h.contains("guarda los cambios")),
            p.header.iter().any(|h| h == MAY_PUBLISH_EARLIER),
        )
    };
    // Snowflake doesn't list the object: no warning, but the note says
    // the COMMIT may publish changes staged before.
    let p = edit_plan(&f, comment).await.unwrap();
    assert_eq!(warned(&p.script()), [NOT_ATOMIC]);
    assert_eq!(staged(&p), (false, true), "{}", p.script());
    // Nor when the read fails.
    f.errors = vec![("SHOW TASK GRAPHS".into(), "HTTP 503".into())];
    let p = edit_plan(&f, comment).await.unwrap();
    assert_eq!(warned(&p.script()), [NOT_ATOMIC]);
    assert_eq!(staged(&p), (false, true), "{}", p.script());
    f.errors.clear();
    let graphs = |pending: &str| {
        set(
            &[
                "database_name",
                "schema_name",
                "name",
                "has_pending_changes",
            ],
            &[&["DB", "ETL", "PIPE", pending]],
        )
    };
    f.answers
        .insert(0, ("SHOW TASK GRAPHS LIKE".into(), graphs("true")));
    let p = edit_plan(&f, comment).await.unwrap();
    assert_eq!(warned(&p.script()), [PENDING_COMMIT, NOT_ATOMIC]);
    assert_eq!(staged(&p), (true, false), "{}", p.script());
    assert!(f
        .seen()
        .iter()
        .any(|s| s == r#"SHOW TASK GRAPHS LIKE 'PIPE' IN SCHEMA "DB"."ETL""#));
    f.answers[0].1 = graphs("false");
    let p = edit_plan(&f, comment).await.unwrap();
    assert_eq!(warned(&p.script()), [NOT_ATOMIC]);
    assert_eq!(staged(&p), (true, false), "{}", p.script());
    // Another graph by that name pattern isn't this one: Snowflake didn't
    // say.
    f.answers[0].1 = set(
        &[
            "database_name",
            "schema_name",
            "name",
            "has_pending_changes",
        ],
        &[&["DB", "OTHER", "PIPE", "false"]],
    );
    let p = edit_plan(&f, comment).await.unwrap();
    assert_eq!(staged(&p), (false, true), "{}", p.script());
    // Without the COMMIT, nothing is published.
    f.answers[0].1 = graphs("true");
    let p = edit_plan(&f, |v| {
        v.values.insert("comment".into(), json!("x"));
        v.values.insert("commit".into(), json!(false));
    })
    .await
    .unwrap();
    assert!(warned(&p.script()).is_empty(), "{}", p.script());
}

#[test]
fn an_alert_change_warns_only_with_several_statements() {
    let a = alert_row();
    let cur = alert::current(&a);
    let mut v = alert_values();
    v.values.insert("enabled".into(), json!(true));
    let want = alert::spec(&v, &choices(), Some(&a)).unwrap();
    let p = alert::alter_plan(&cur, &want);
    assert_eq!(p.steps.len(), 1);
    assert!(warned(&p.script()).is_empty(), "{}", p.script());
    v.values.insert("comment".into(), json!("new"));
    let want = alert::spec(&v, &choices(), Some(&a)).unwrap();
    assert_eq!(
        warned(&alert::alter_plan(&cur, &want).script()),
        [NOT_ATOMIC]
    );
    // Created suspended, then resumed: two statements.
    v.values.insert("name".into(), json!("new_alert"));
    let s = alert::spec(&v, &choices(), None).unwrap();
    assert_eq!(warned(&alert::create_plan(&s).script()), [NOT_ATOMIC]);
    v.values.insert("enabled".into(), json!(false));
    let s = alert::spec(&v, &choices(), None).unwrap();
    assert!(warned(&alert::create_plan(&s).script()).is_empty());
    assert!(warned(&alert::drop_plan(&a).script()).is_empty());
}
