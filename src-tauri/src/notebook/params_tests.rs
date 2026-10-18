//! Tests for notebook parameters (`notebook::params`): the `{{name}}`
//! syntax per language, quoting per dialect, values that try to escape
//! their literal, names refused, the bound form and the file's round trip.
//! The live ones (`dbine-test-*` containers) are in `params_live_tests.rs`.

use super::*;
use crate::notebook::{self, CellResult, NotebookTarget, PreviewLimits, StoredColumn, StoredStatement};
use serde_json::json;
use std::sync::Arc;

fn p(name: &str, kind: ParamType) -> NotebookParameter {
    NotebookParameter { name: name.into(), kind, default: None, description: None, required: false, options: vec![] }
}

fn defs() -> Vec<NotebookParameter> {
    vec![
        NotebookParameter { required: true, ..p("mes", ParamType::Text) },
        NotebookParameter { default: Some(json!(10)), ..p("tope", ParamType::Number) },
        p("desde", ParamType::Date),
        p("activo", ParamType::Boolean),
        NotebookParameter { options: vec!["Norte".into(), "Sur".into()], ..p("region", ParamType::List) },
    ]
}

fn driver(id: &str) -> &'static Arc<dyn dbine_driver::Driver> {
    dbine_drivers::find(id).unwrap_or_else(|| panic!("driver {id}"))
}

fn values(given: serde_json::Value) -> BTreeMap<String, ParamValue> {
    let given: Values = serde_json::from_value(given).unwrap();
    resolve(&defs(), &given).unwrap()
}

fn prep(id: &str, text: &str, given: serde_json::Value) -> Result<Prepared, ParamError> {
    prepare_text(text, &Engine::Driver(driver(id).as_ref()), &defs(), &values(given), None)
}

fn lit(id: &str, text: &str, given: serde_json::Value) -> String {
    prep(id, text, given).unwrap().text
}

#[test]
fn markers_in_code_run_and_in_comments_stay() {
    let v = json!({ "mes": "2026-10" });
    assert_eq!(lit("postgres", "SELECT * FROM v WHERE mes = {{mes}} AND n < {{ tope }}", v.clone()), "SELECT * FROM v WHERE mes = E'2026-10' AND n < 10");
    // Comments keep them as text.
    for text in ["SELECT 1 -- {{mes}}", "SELECT 1 /* {{mes}} */", "SELECT 1\n-- WHERE x = {{nada}}"] {
        assert_eq!(lit("postgres", text, v.clone()), text);
    }
    assert_eq!(lit("mysql", "SELECT 1 # {{mes}}", v.clone()), "SELECT 1 # {{mes}}");
    // Names are case-insensitive, written as defined in the result.
    let r = prep("postgres", "SELECT {{MES}}, {{mes}}", v.clone()).unwrap();
    assert_eq!(r.text, "SELECT E'2026-10', E'2026-10'");
    assert_eq!(r.used, vec!["mes"]);
    // A template's triple braces are not ours.
    assert!(all_markers("x {{{mes}}} y").is_empty());
    assert_eq!(all_markers("{a: {{mes}}}").len(), 1);
    let _ = v;
}

#[test]
fn inside_strings_and_quoted_names_is_refused() {
    let v = json!({ "mes": "x" });
    let in_string = |id: &str, t: &str| matches!(prep(id, t, v.clone()), Err(ParamError::InString { .. }));
    let ident = |id: &str, t: &str| matches!(prep(id, t, v.clone()), Err(ParamError::Identifier { .. }));
    assert!(in_string("postgres", "SELECT * FROM t WHERE a LIKE '%{{mes}}%'"));
    assert!(in_string("postgres", "SELECT E'{{mes}}'"));
    assert!(in_string("postgres", "SELECT $$ {{mes}} $$"));
    assert!(in_string("sqlserver", "SELECT N'{{mes}}'"));
    assert!(in_string("oracle", "SELECT q'[{{mes}}]' FROM dual"));
    assert!(in_string("mysql", "SELECT \"{{mes}}\""));
    assert!(ident("postgres", "SELECT \"{{mes}}\" FROM t"));
    assert!(ident("sqlserver", "SELECT [{{mes}}] FROM t"));
    assert!(ident("mysql", "SELECT `{{mes}}` FROM t"));
    assert!(in_string("mongodb", "db.c.find({ a: \"{{mes}}\" })"));
    // The message says what to do.
    let e = prep("postgres", "SELECT '%{{mes}}%'", v).unwrap_err().to_string();
    assert!(e.contains("«mes»") && e.contains("'%' || {{mes}} || '%'"), "{e}");
}

#[test]
fn where_a_name_goes_is_refused() {
    let v = json!({ "mes": "x" });
    for text in [
        "SELECT * FROM {{mes}}",
        "SELECT * FROM t JOIN {{mes}} ON true",
        "SELECT t.{{mes}} FROM t",
        "SELECT * FROM {{mes}}.t",
        "SELECT {{mes}}(1)",
        "SELECT * FROM tabla_{{mes}}",
        "SELECT * FROM t{{mes}}",
        "SELECT {{mes}}_x FROM t",
        "SELECT * FROM t ORDER BY {{mes}}",
        "SELECT a AS {{mes}} FROM t",
        "UPDATE t SET {{mes}} = 1",
        "INSERT INTO {{mes}} VALUES (1)",
        "EXEC {{mes}}",
    ] {
        assert!(matches!(prep("postgres", text, v.clone()), Err(ParamError::Identifier { .. })), "{text}");
    }
    for (id, text) in [
        ("mongodb", "db.{{mes}}.find({})"),
        ("mongodb", "db.c.find({ {{mes}}: 1 })"),
        ("redis", "{{mes}} k"),
        ("redis", "GET k\n  {{mes}} k"),
        ("neo4j", "MATCH (n:{{mes}}) RETURN n"),
        ("neo4j", "MATCH ()-[:{{mes}}]->() RETURN 1"),
    ] {
        assert!(matches!(prep(id, text, v.clone()), Err(ParamError::Identifier { .. })), "{id}: {text}");
    }
    // Values are fine there.
    for (id, text) in [
        ("postgres", "SELECT * FROM t WHERE a IN ({{mes}}, 'b') AND b BETWEEN {{tope}} AND {{tope}}"),
        ("postgres", "SELECT CAST({{mes}} AS text), {{mes}} AS m LIMIT {{tope}}"),
        ("sqlserver", "SELECT TOP ({{tope}}) * FROM t WHERE a = {{mes}}"),
        ("mongodb", "db.c.find({ a: {{mes}}, b: { $gt: {{tope}} } })"),
        ("redis", "GET {{mes}}"),
        ("neo4j", "MATCH (n {name: {{mes}}}) WHERE n.x = {{tope}} RETURN n"),
    ] {
        assert!(prep(id, text, v.clone()).is_ok(), "{id}: {text}");
    }
}

#[test]
fn undefined_names_are_an_error_in_sql_only() {
    let v = json!({ "mes": "x" });
    assert!(matches!(prep("postgres", "SELECT {{mse}}", v.clone()), Err(ParamError::Undefined { name, .. }) if name == "mse"));
    // A search template's own `{{x}}` (Elasticsearch, in its JSON) stays.
    let es = r#"{"query": {"match": {"title": "{{query_string}}"}}}"#;
    assert_eq!(lit("elasticsearch", es, v.clone()), es);
    assert_eq!(lit("mongodb", "db.c.find({a: {{otro}}})", v), "db.c.find({a: {{otro}}})");
}

#[test]
fn values_are_quoted_per_dialect() {
    let v = json!({ "mes": "O'Brien", "desde": "2026-10-01", "activo": true });
    assert_eq!(lit("postgres", "SELECT {{mes}}, {{desde}}, {{activo}}", v.clone()), "SELECT E'O''Brien', DATE '2026-10-01', TRUE");
    assert_eq!(lit("sqlserver", "SELECT {{mes}}, {{desde}}, {{activo}}", v.clone()), "SELECT N'O''Brien', CAST('2026-10-01' AS date), 1");
    assert_eq!(lit("oracle", "SELECT {{mes}}, {{desde}}, {{activo}} FROM dual", v.clone()), "SELECT 'O''Brien', DATE '2026-10-01', 1 FROM dual");
    assert_eq!(lit("mysql", "SELECT {{mes}}", json!({ "mes": "a\\b'c" })), "SELECT 'a\\\\b''c'");
    assert_eq!(lit("postgres", "SELECT {{mes}}", json!({ "mes": "a\\b" })), "SELECT E'a\\\\b'");
    assert_eq!(lit("mongodb", "db.c.find({a: {{mes}}, d: {{desde}}})", v.clone()), "db.c.find({a: \"O'Brien\", d: ISODate(\"2026-10-01T00:00:00Z\")})");
    assert_eq!(lit("redis", "GET {{mes}}", v.clone()), "GET \"O'Brien\"");
    assert_eq!(lit("cassandra", "SELECT * FROM t WHERE a = {{mes}}", v), "SELECT * FROM t WHERE a = 'O''Brien'");
    // NULL where the language has it; a clear error where it doesn't.
    assert_eq!(lit("postgres", "SELECT {{desde}}", json!({ "mes": "x" })), "SELECT NULL");
    assert!(matches!(prep("redis", "GET {{desde}}", json!({ "mes": "x" })), Err(ParamError::Literal { .. })));
    // `a -{{n}}` with a negative number is never a `--` comment.
    assert_eq!(lit("postgres", "SELECT 1 -{{tope}}", json!({ "mes": "x", "tope": -5 })), "SELECT 1 - -5");
}

/// What a value can hold to break out of its literal stays inside it: the
/// rendered text is still one statement, and the value one string token.
#[test]
fn injection_attempts_stay_data() {
    let evil = [
        "'; DROP TABLE x; --",
        "\\'; DROP TABLE x; --",
        "x' OR '1'='1",
        "$$; DROP TABLE x; $$",
        "a\nGO\nDROP TABLE x",
        "*/ DROP TABLE x; /*",
        "]; DROP TABLE x; --",
        "\"}); db.x.drop(); ({\"",
    ];
    for id in ["postgres", "sqlserver", "mysql", "oracle", "sqlite", "clickhouse", "duckdb"] {
        let d = driver(id);
        for e in evil {
            let text = lit(id, "SELECT * FROM t WHERE a = {{mes}}", json!({ "mes": e }));
            let units = d.split_script(&text);
            assert_eq!(units.len(), 1, "{id}: {e:?} -> {text}");
            let toks = dbine_driver::sql::name_tokens(&text, &d.script_dialect());
            let last = toks.last().unwrap();
            assert_eq!(last.kind, TokenKind::String, "{id}: {e:?} -> {text}");
            assert_eq!(last.end, text.len(), "{id}: {e:?} -> {text}");
        }
    }
    for e in evil {
        let text = lit("mongodb", "db.c.find({a: {{mes}}})", json!({ "mes": e }));
        assert_eq!(text, format!("db.c.find({{a: {}}})", serde_json::to_string(e).unwrap()));
        let text = lit("redis", "GET {{mes}}", json!({ "mes": e }));
        assert!(!text.contains('\n'), "{text}");
    }
    // Numbers, dates and booleans can't hold anything but themselves.
    let bad = [json!({ "mes": "x", "tope": "1; DROP TABLE x" }), json!({ "mes": "x", "desde": "2026-10-01'; --" }), json!({ "mes": "x", "activo": "1 OR 1=1" })];
    for b in bad {
        let given: Values = serde_json::from_value(b).unwrap();
        assert!(matches!(resolve(&defs(), &given), Err(ParamError::Invalid { .. })));
    }
}

#[test]
fn the_bound_form_keeps_values_apart() {
    let r = prep("sqlserver", "SELECT * FROM t WHERE a = {{mes}} OR b = {{mes}}\nGO\nSELECT {{tope}}", json!({ "mes": "'; DROP TABLE x; --" })).unwrap();
    assert_eq!(r.bound.len(), 2);
    let first = r.bound_unit(0).unwrap();
    assert_eq!(first.numbered("@P"), "SELECT * FROM t WHERE a = @P1 OR b = @P1");
    assert_eq!(first.values, vec![ParamValue::Text("'; DROP TABLE x; --".into())]);
    assert_eq!(r.bound_unit(1).unwrap().numbered("@P"), "SELECT @P1");
    // A unit without parameters runs as text.
    let r = prep("postgres", "SELECT 1; SELECT {{mes}}", json!({ "mes": "x" })).unwrap();
    assert!(r.bound_unit(0).is_none());
    assert_eq!(r.bound_unit(1).unwrap().numbered("$"), "SELECT $1");
    // No parameters: nothing to bind, the text as it is.
    let r = prep("postgres", "SELECT '{{x}}' -- {{y}}", json!({ "mes": "x" })).unwrap();
    assert_eq!(r.text, "SELECT '{{x}}' -- {{y}}");
    assert!(r.bound.is_empty() && r.used.is_empty());
}

#[test]
fn values_resolve_with_defaults_and_requirements() {
    let given: Values = serde_json::from_value(json!({})).unwrap();
    assert_eq!(resolve(&defs(), &given), Err(ParamError::Missing { names: vec!["mes".into()] }));
    let v = values(json!({ "mes": "x", "region": "Sur", "desde": "", "activo": "sí" }));
    assert_eq!(v["tope"], ParamValue::Number("10".into()));
    assert_eq!(v["desde"], ParamValue::Null);
    assert_eq!(v["region"], ParamValue::Text("Sur".into()));
    assert_eq!(v["activo"], ParamValue::Bool(true));
    let given: Values = serde_json::from_value(json!({ "mes": "x", "region": "Este" })).unwrap();
    assert!(matches!(resolve(&defs(), &given), Err(ParamError::Invalid { name, .. }) if name == "region"));
    let given: Values = serde_json::from_value(json!({ "MES": "x", "tope": 3 })).unwrap();
    assert_eq!(resolve(&defs(), &given).unwrap()["tope"], ParamValue::Number("3".into()));
}

fn nb_with(cells: Vec<NotebookCell>) -> Notebook {
    Notebook {
        target: Some(NotebookTarget { connection: Some("Ventas".into()), database: None, environment: None, results: false }),
        save_results: true,
        save_results_set: true,
        cells,
        parameters: defs(),
        origin: "dbine".into(),
        extra_metadata: json!({}),
        warnings: vec![],
    }
}

fn cell(id: &str, source: &str, dataframes: bool) -> NotebookCell {
    NotebookCell {
        id: id.into(),
        kind: CellKind::Sql,
        source: source.into(),
        language: Some("sql".into()),
        target: dataframes.then(|| NotebookTarget { results: true, ..Default::default() }),
        keep_result: true,
        chart: None,
        execution_count: None,
        result: None,
        raw: None,
        name: Some(id.into()),
    }
}

#[test]
fn the_file_keeps_definitions_and_the_values_of_each_result() {
    let mut nb = nb_with(vec![cell("ventas", "SELECT * FROM v WHERE mes = {{mes}}", false)]);
    nb.parameters.push(NotebookParameter { default: Some(json!("s3cr3t")), ..p("api_token", ParamType::Text) });
    nb.parameters.push(NotebookParameter { default: Some(json!("Server=db;Password=x")), description: Some("  ".into()), ..p("origen", ParamType::Text) });
    let mut params = Map::new();
    params.insert("mes".into(), json!("2026-10"));
    nb.cells[0].result = Some(CellResult {
        results: vec![StoredStatement {
            columns: vec![StoredColumn { name: "n".into(), type_name: String::new() }],
            rows: vec![vec![json!(1)]],
            rows_total: 1,
            rows_kept: 1,
            truncated: false,
            rows_affected: None,
            statement: None,
        }],
        ran_at: "2026-10-18T14:32:05Z".into(),
        params: Some(params.clone()),
        ..Default::default()
    });
    let text = notebook::serialize(&nb, PreviewLimits::new(None, None));
    let v: Value = serde_json::from_str(&text).unwrap();
    let ps = v["metadata"]["dbine"]["parameters"].as_array().unwrap();
    assert_eq!(ps[0], json!({ "name": "mes", "type": "text", "required": true }));
    assert_eq!(ps[4], json!({ "name": "region", "type": "list", "options": ["Norte", "Sur"] }));
    // No credential or connection string as a default.
    assert!(!text.contains("s3cr3t") && !text.contains("Password=x"));
    assert_eq!(v["cells"][0]["outputs"][0]["metadata"]["dbine"]["params"], json!({ "mes": "2026-10" }));
    assert_eq!(v["cells"][0]["metadata"]["dbine"]["run"]["params"], json!({ "mes": "2026-10" }));
    assert!(v["cells"][0]["outputs"][0]["data"]["text/plain"].as_str().unwrap().contains("mes = 2026-10"));

    let back = notebook::parse(&text).unwrap();
    assert_eq!(back.parameters.len(), 7);
    assert_eq!(back.parameters[..5], defs()[..]);
    assert_eq!(back.parameters[5].default, None);
    assert_eq!(back.cells[0].result.as_ref().unwrap().params, Some(params));

    // A file written elsewhere with a credential as a default: dropped, with the warning.
    let mut raw: Value = serde_json::from_str(&text).unwrap();
    raw["metadata"]["dbine"]["parameters"][0]["default"] = json!("x");
    raw["metadata"]["dbine"]["parameters"][5]["default"] = json!("s3cr3t");
    raw["metadata"]["dbine"]["parameters"][6] = json!({ "name": "MES", "type": "number" });
    let back = notebook::parse(&raw.to_string()).unwrap();
    assert_eq!(back.parameters[0].default, Some(json!("x")));
    assert_eq!(back.parameters[5].default, None);
    assert_eq!(back.parameters.len(), 6, "a repeated name is dropped");
    assert!(!back.warnings.is_empty());

    // A file without parameters opens unchanged.
    let old = r#"{"cells":[],"metadata":{"dbine":{"version":1,"save_results":true}},"nbformat":4,"nbformat_minor":5}"#;
    let nb = notebook::parse(old).unwrap();
    assert!(nb.parameters.is_empty());
    assert!(notebook::serialize(&nb, PreviewLimits::new(None, None)).find("parameters").is_none());
}

#[test]
fn a_whole_notebook_prepares_for_a_run_without_the_ui() {
    let mut nb = nb_with(vec![
        cell("ventas", "SELECT * FROM v WHERE mes = {{mes}} AND n < {{tope}}", false),
        cell("vacia", "  ", false),
        cell("resumen", "SELECT count(*) FROM ventas WHERE region = {{region}}", true),
        cell("docs", "db.c.find({ mes: {{mes}} })", false),
    ]);
    nb.cells[3].target = Some(NotebookTarget { connection: Some("Mongo".into()), ..Default::default() });
    let resolve_driver = |c: &NotebookCell| -> Option<Arc<dyn dbine_driver::Driver>> {
        let name = c.target.as_ref().and_then(|t| t.connection.as_deref()).unwrap_or("Ventas");
        Some(driver(if name == "Mongo" { "mongodb" } else { "postgres" }).clone())
    };
    let given: Values = serde_json::from_value(json!({ "mes": "2026-10", "region": "Norte" })).unwrap();
    let cells = prepare_notebook(&nb, &given, &resolve_driver).unwrap();
    assert_eq!(cells.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["ventas", "resumen", "docs"]);
    assert_eq!(cells[0].prepared.text, "SELECT * FROM v WHERE mes = E'2026-10' AND n < 10");
    assert_eq!(cells[0].prepared.bound_unit(0).unwrap().numbered("$"), "SELECT * FROM v WHERE mes = $1 AND n < $2");
    assert_eq!(cells[0].values, serde_json::from_value::<Map<String, Value>>(json!({ "mes": "2026-10", "tope": 10 })).unwrap());
    assert!(cells[1].dataframes);
    assert_eq!(cells[1].prepared.text, "SELECT count(*) FROM ventas WHERE region = 'Norte'");
    assert_eq!(cells[2].prepared.text, "db.c.find({ mes: \"2026-10\" })");

    // A required value missing: nothing is prepared.
    assert!(matches!(prepare_notebook(&nb, &Values::new(), &resolve_driver), Err(ParamError::Missing { .. })));
    // A cell that uses one where a name goes: the error names the cell.
    nb.cells.push(cell("malo", "SELECT * FROM {{mes}}", false));
    let e = prepare_notebook(&nb, &given, &resolve_driver).unwrap_err();
    assert!(matches!(&e, ParamError::Identifier { cell: Some(c), .. } if c == "malo"), "{e:?}");
    assert!(e.to_string().contains("celda malo"));
}

#[test]
fn mentioned_lists_the_defined_ones() {
    assert_eq!(mentioned("SELECT {{mes}}, {{tope}}, '{{mes}}', {{nada}} -- {{region}}", &defs()), vec!["mes", "tope", "region"]);
}
