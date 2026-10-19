use super::super::{parse, CellResult, NotebookCell, StoredColumn, StoredStatement};
use super::*;
use dbine_driver::FieldKind;
use std::collections::BTreeMap;

/// Every secret value the tests put in a config: none may reach the export.
const MARK: &str = "ZQXSECRET";

fn cell(id: &str, kind: CellKind, source: &str, target: Option<NotebookTarget>, name: Option<&str>) -> NotebookCell {
    NotebookCell {
        id: id.into(),
        kind,
        source: source.into(),
        language: None,
        target,
        name: name.map(String::from),
        keep_result: true,
        chart: None,
        execution_count: None,
        result: None,
        raw: None,
        write: None,
        write_result: None,
    }
}

fn on(conn: &str) -> Option<NotebookTarget> {
    Some(NotebookTarget { connection: Some(conn.into()), ..Default::default() })
}

fn dataframes() -> Option<NotebookTarget> {
    Some(NotebookTarget { results: true, ..Default::default() })
}

fn empty_nb() -> Notebook {
    parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[]}"#).unwrap()
}

/// A config with secrets everywhere they can be: the password, the driver's
/// secret options, the SSH tunnel's, a secret manager's definitions, ODBC's
/// extra attributes and a URL with credentials in the host.
fn cfg(driver: &str, host: &str, port: u16, database: &str, user: &str, opts: &[(&str, &str)]) -> ConnectionConfig {
    let mut options: BTreeMap<String, String> = opts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    options.insert("ssh.password".into(), format!("{MARK}-ssh"));
    options.insert("ssh.passphrase".into(), format!("{MARK}-passphrase"));
    options.insert("extra".into(), format!("PWD={MARK}-extra"));
    options.insert("connection_string".into(), format!("Server=x;Password={MARK}-cs"));
    options.insert("token".into(), format!("{MARK}-token"));
    options.insert("api_key".into(), format!("{MARK}-apikey"));
    options.insert("source_token:password:token".into(), format!("{MARK}-vault-token"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: database.into(),
        username: Some(user.into()),
        password: Some(format!("{MARK}-password")),
        options,
        ..Default::default()
    }
}

fn conn(name: &str, c: ConnectionConfig) -> PyConn {
    let info = dbine_drivers::find(&c.driver).map(|d| d.info());
    PyConn::new(&format!("id-{name}"), name, &c, info, "")
}

fn sources(v: &Value) -> Vec<String> {
    v["cells"].as_array().unwrap().iter().map(|c| c["source"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect::<String>()).collect()
}

/// The notebook the task names: PostgreSQL, SQL Server, MySQL, Oracle,
/// MongoDB, SQLite, DuckDB, a DataFrame SQL join and text cells.
fn multi_engine() -> (Notebook, HashMap<String, PyConn>) {
    let mut nb = empty_nb();
    nb.cells = vec![
        cell("intro", CellKind::Markdown, "# Ventas por región\n\nCruce de **cinco** motores.", None, None),
        cell("pg", CellKind::Sql, "SELECT region, sum(total) AS total\nFROM ventas\nWHERE fecha > now() - interval '1 day' AND hora = '10:30'\nGROUP BY region", on("Ventas PG"), Some("ventas")),
        cell("ms", CellKind::Sql, "SELECT id, nombre FROM dbo.clientes WHERE nombre LIKE N'%\"x\"%'", on("CRM MSSQL"), Some("clientes")),
        cell("my", CellKind::Sql, "SELECT * FROM pedidos", on("Pedidos MySQL"), Some("pedidos")),
        cell("ora", CellKind::Sql, "SELECT * FROM hr.employees", on("RRHH Oracle"), Some("empleados")),
        cell("mongo", CellKind::Sql, "db.regiones.find({\"activo\": true}, {\"_id\": 0}).sort({\"nombre\": 1}).limit(50)", on("Mongo"), Some("regiones")),
        cell("lite", CellKind::Sql, "SELECT * FROM zonas", on("Zonas SQLite"), Some("zonas")),
        cell("duck", CellKind::Sql, "SELECT * FROM metas", on("Metas DuckDB"), Some("metas")),
        cell("note", CellKind::Markdown, "Ahora el cruce:", None, None),
        cell(
            "join",
            CellKind::Sql,
            "SELECT c.nombre, v.region, v.total\nFROM ventas v\nJOIN clientes c ON c.id = v.cliente\nJOIN read_csv('datos/metas.csv') m ON m.region = v.region",
            dataframes(),
            Some("cruce"),
        ),
    ];
    let mut conns = HashMap::new();
    let pg = conn("Ventas PG", cfg("postgres", "pg.example.com", 5432, "ventas", "app", &[("auth", "password")]));
    let ms = conn("CRM MSSQL", cfg("sqlserver", "sql.example.com", 1433, "crm", "sa", &[]));
    let my = conn("Pedidos MySQL", cfg("mysql", "my.example.com", 3306, "tienda", "root", &[]));
    let ora = conn("RRHH Oracle", cfg("oracle", "ora.example.com", 1521, "", "hr", &[("connect_by", "service"), ("service", "ORCLPDB1")]));
    let mongo = conn("Mongo", cfg("mongodb", &format!("mongodb+srv://u:{MARK}-uri@cluster0.example.net/?authSource=admin&tlsCertificateKeyFilePassword={MARK}-q"), 0, "geo", "lector", &[("auth_source", "admin")]));
    let lite = conn("Zonas SQLite", cfg("sqlite", "/data/zonas.db", 0, "", "", &[]));
    let duck = conn("Metas DuckDB", cfg("duckdb", "/data/metas.duckdb", 0, "", "", &[]));
    for (id, c) in [("pg", pg), ("ms", ms), ("my", my), ("ora", ora), ("mongo", mongo), ("lite", lite), ("duck", duck)] {
        conns.insert(id.to_string(), c);
    }
    (nb, conns)
}

#[test]
fn exports_a_multi_engine_notebook() {
    let (nb, conns) = multi_engine();
    let text = python(&nb, &conns, &PyExport::default());
    assert!(!text.contains(MARK), "a secret got out:\n{text}");
    let v: Value = serde_json::from_str(&text).expect("valid JSON");
    let all = sources(&v).join("\n----\n");

    // The setup: one client per connection, passwords from the environment.
    assert!(all.contains("from sqlalchemy import URL, create_engine, text"), "{all}");
    assert!(all.contains("engine_ventas_pg = create_engine(\n    URL.create(\n        \"postgresql+psycopg\",\n        username=\"app\",\n        password=os.environ[\"DBINE_VENTAS_PG_PASSWORD\"],\n        host=\"pg.example.com\",\n        port=5432,\n        database=\"ventas\","), "{all}");
    assert!(all.contains("\"mssql+pymssql\""));
    assert!(all.contains("\"mysql+pymysql\""));
    assert!(all.contains("\"oracle+oracledb\"") && all.contains("query={\"service_name\": \"ORCLPDB1\"}"));
    assert!(all.contains("mongo_mongo = MongoClient(host=\"mongodb+srv://cluster0.example.net/\", username=\"lector\", password=os.environ[\"DBINE_MONGO_PASSWORD\"], authSource=\"admin\")[\"geo\"]"), "{all}");
    assert!(all.contains("\"sqlite\",\n        database=\"/data/zonas.db\","));
    assert!(all.contains("duck_metas_duckdb = duckdb.connect(\"/data/metas.duckdb\", read_only=True)"));

    // The cells.
    assert!(all.contains("ventas = pd.read_sql(text(\"\"\"SELECT region, sum(total) AS total\nFROM ventas\nWHERE fecha > now() - interval '1 day' AND hora = '10:30'\nGROUP BY region\"\"\"), engine_ventas_pg)\nventas"), "{all}");
    assert!(all.contains("clientes = pd.read_sql(text(\"\"\"SELECT id, nombre FROM dbo.clientes WHERE nombre LIKE N'%\"x\"%'\"\"\"), engine_crm_mssql)"), "{all}");
    assert!(all.contains("regiones = pd.DataFrame(list(mongo_mongo[\"regiones\"].find({\"activo\": True}, {\"_id\": 0}).sort([(\"nombre\", 1)]).limit(50)))"), "{all}");
    assert!(all.contains("zonas = pd.read_sql(text(\"\"\"SELECT * FROM zonas\"\"\"), engine_zonas_sqlite)"));
    assert!(all.contains("metas = duck_metas_duckdb.sql(\"\"\"SELECT * FROM metas\"\"\").df()"));
    assert!(all.contains("cruce = duckdb.sql(\"\"\"SELECT c.nombre, v.region, v.total\nFROM ventas v\nJOIN clientes c ON c.id = v.cliente\nJOIN read_csv('datos/metas.csv') m ON m.region = v.region\"\"\").df()\ncruce"), "{all}");

    // Instructions: packages and variables.
    let intro = &sources(&v)[0];
    assert!(intro.starts_with("# Ventas por región (Python)"), "{intro}");
    for p in ["pandas", "sqlalchemy", "\"psycopg[binary]\"", "pymssql", "pymysql", "oracledb", "pymongo", "duckdb"] {
        assert!(intro.contains(p), "{p} missing from {intro}");
    }
    for e in ["DBINE_VENTAS_PG_PASSWORD", "DBINE_CRM_MSSQL_PASSWORD", "DBINE_PEDIDOS_MYSQL_PASSWORD", "DBINE_RRHH_ORACLE_PASSWORD", "DBINE_MONGO_PASSWORD"] {
        assert!(intro.contains(&format!("`{e}`")), "{e} missing from {intro}");
    }
    assert!(!intro.contains("ZONAS_SQLITE"), "SQLite has no password");
    // Text cells stay Markdown.
    let md: Vec<&Value> = v["cells"].as_array().unwrap().iter().filter(|c| c["cell_type"] == "markdown").collect();
    assert_eq!(md.len(), 3);
    assert!(sources(&v).iter().any(|s| s == "# Ventas por región\n\nCruce de **cinco** motores."));
}

#[test]
fn the_ipynb_has_nbformat_4_5_shape() {
    let (nb, conns) = multi_engine();
    let v: Value = serde_json::from_str(&python(&nb, &conns, &PyExport::default())).unwrap();
    assert_eq!(v["nbformat"], 4);
    assert_eq!(v["nbformat_minor"], 5);
    assert_eq!(v["metadata"]["kernelspec"]["name"], "python3");
    assert_eq!(v["metadata"]["language_info"]["name"], "python");
    assert!(v["metadata"].get("dbine").is_none(), "not a DBine notebook");
    let mut ids = HashSet::new();
    for c in v["cells"].as_array().unwrap() {
        let id = c["id"].as_str().unwrap();
        assert!(!id.is_empty() && id.len() <= 64 && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'), "{id}");
        assert!(ids.insert(id.to_string()), "duplicate id {id}");
        assert!(c["metadata"].is_object());
        assert!(c["source"].as_array().unwrap().iter().all(Value::is_string));
        match c["cell_type"].as_str().unwrap() {
            "code" => {
                assert!(c["outputs"].as_array().unwrap().is_empty(), "clean notebook");
                assert!(c["execution_count"].is_null());
            }
            "markdown" => assert!(c.get("outputs").is_none()),
            t => panic!("cell type {t}"),
        }
    }
}

#[test]
fn previews_only_when_asked() {
    let (mut nb, conns) = multi_engine();
    nb.cells[1].execution_count = Some(3);
    nb.cells[1].result = Some(CellResult {
        results: vec![StoredStatement {
            columns: vec![StoredColumn { name: "region".into(), type_name: String::new() }],
            rows: (0..5).map(|i| vec![json!(format!("r{i}"))]).collect(),
            rows_total: 5,
            rows_kept: 5,
            truncated: false,
            rows_affected: None,
            statement: None,
        }],
        messages: vec![],
        error: None,
        ran_at: "2026-10-18T14:32:05Z".into(),
        duration_ms: 12,
        engine: Some("postgres".into()),
        target_label: Some("Ventas PG › ventas".into()),
        truncated: false,
        params: None,
    });
    let clean: Value = serde_json::from_str(&python(&nb, &conns, &PyExport::default())).unwrap();
    assert!(clean["cells"].as_array().unwrap().iter().all(|c| c.get("outputs").is_none_or(|o| o.as_array().unwrap().is_empty())));
    let opts = PyExport { previews: true, limits: PreviewLimits::new(Some(2), None), ..Default::default() };
    let with: Value = serde_json::from_str(&python(&nb, &conns, &opts)).unwrap();
    let pg = with["cells"].as_array().unwrap().iter().find(|c| c["id"] == "pg").unwrap();
    assert_eq!(pg["execution_count"], 3);
    let out = &pg["outputs"][0];
    assert_eq!(out["output_type"], "execute_result");
    assert_eq!(out["metadata"], json!({}));
    let html = out["data"]["text/html"].as_str().unwrap();
    assert!(html.contains("r1") && !html.contains("r2"), "the preview limits apply: {html}");
}

#[test]
fn no_driver_leaks_a_secret_and_every_one_is_mapped_or_explained() {
    let mut table = Vec::new();
    for d in dbine_drivers::all() {
        let info = d.info();
        let mut c = ConnectionConfig { driver: info.id.into(), ..Default::default() };
        for f in &info.fields {
            let secret = format!("{MARK}-{}", f.key);
            let plain = match f.kind {
                FieldKind::Bool => "true".to_string(),
                FieldKind::Number => "1234".to_string(),
                _ => format!("v_{}", f.key),
            };
            let value = if f.secret { secret } else { plain };
            match f.key {
                "host" => c.host = if f.secret { value } else { "db.example.com".into() },
                "port" => c.port = 1234,
                "database" => c.database = value,
                "username" => c.username = Some(value),
                "password" => c.password = Some(value),
                "encrypt" => c.encrypt = true,
                "trust_server_certificate" | "read_only" => {}
                k => {
                    c.options.insert(k.to_string(), value);
                }
            }
        }
        for (k, v) in [("ssh.enabled", "true"), ("ssh.host", "bastion.example.com"), ("ssh.user", "jump"), ("ssh.password", "ZQXSECRET-ssh"), ("ssh.passphrase", "ZQXSECRET-pp")] {
            c.options.insert(k.into(), v.into());
        }
        c.options.insert(
            "secret_sources".into(),
            format!(r#"{{"password":{{"kind":"vault","address":"https://vault.example.com","path":"kv/data/{MARK}-path","key":"password"}}}}"#),
        );
        let pc = PyConn::new("id", &format!("Conn {}", info.id), &c, Some(info), "");
        let mut nb = empty_nb();
        nb.cells = vec![cell("q", CellKind::Sql, "SELECT 1", on("x"), Some("q"))];
        let text = python(&nb, &HashMap::from([("q".to_string(), pc.clone())]), &PyExport::default());
        assert!(!text.contains(MARK), "{} leaks a secret:\n{text}", info.id);
        let v: Value = serde_json::from_str(&text).unwrap();
        let q = sources(&v).into_iter().last().unwrap();
        let (what, reason) = mapping_of(&pc);
        if reason.is_some() || what.contains("commented") {
            assert!(q.starts_with("# TODO: not exported as Python"), "{}: {q}", info.id);
        } else if what != "pymongo" && what != "requests" {
            assert!(q.starts_with("q = ") && q.ends_with("\nq"), "{}: {q}", info.id);
        }
        table.push(format!("{} | {what} | {}", info.id, reason.unwrap_or_default()));
    }
    // `cargo test … -- --nocapture` prints the mapping (docs/notebooks.md).
    table.sort();
    println!("{}", table.join("\n"));
}

#[test]
fn hosts_and_options_lose_their_credentials() {
    assert_eq!(strip_url_credentials("https://elastic:pw@es.example.com:9200/x?token=abc#f"), "https://es.example.com:9200/x");
    assert_eq!(strip_url_credentials("mongodb+srv://u:p@c.example.net/?authSource=admin"), "mongodb+srv://c.example.net/");
    assert_eq!(strip_url_credentials("/data/a.db"), "/data/a.db");
    let c = cfg("postgres", "pg", 5432, "d", "u", &[("schema", "token=abc"), ("role", "analyst"), ("endpoint_url", "https://k:s@ep.example.com/?sig=1")]);
    let pc = conn("x", c);
    assert_eq!(pc.options.get("role").map(String::as_str), Some("analyst"));
    assert!(!pc.options.contains_key("schema"), "a value that looks like a credential is dropped");
    assert_eq!(pc.options.get("endpoint_url").map(String::as_str), Some("https://ep.example.com/"));
    assert!(!pc.options.contains_key("token") && !pc.options.contains_key("extra") && !pc.options.contains_key("connection_string"));
}

#[test]
fn tunnels_secret_managers_and_cloud_sign_in_become_notes() {
    let mut c = cfg("postgres", "10.0.0.5", 5432, "ventas", "app", &[("auth", "aws_iam"), ("ssh.enabled", "true"), ("ssh.host", "bastion.example.com"), ("ssh.user", "ops")]);
    c.options.insert("secret_sources".into(), format!(r#"{{"password":{{"kind":"aws","secret":"prod/{MARK}"}}}}"#));
    let mut nb = empty_nb();
    nb.cells = vec![cell("q", CellKind::Sql, "SELECT 1", on("Prod"), Some("q"))];
    let text = python(&nb, &HashMap::from([("q".to_string(), conn("Prod", c))]), &PyExport::default());
    assert!(!text.contains(MARK));
    let v: Value = serde_json::from_str(&text).unwrap();
    let intro = &sources(&v)[0];
    assert!(intro.contains("ssh -N -L 5432:10.0.0.5:5432 ops@bastion.example.com -p 22"), "{intro}");
    assert!(intro.contains("signs in with AWS IAM"), "{intro}");
    assert!(intro.contains("secret manager"), "{intro}");
}

#[test]
fn unmappable_and_unresolved_cells_are_commented() {
    let mut nb = empty_nb();
    nb.cells = vec![
        cell("m", CellKind::Sql, "db.c.find({activo: true})", on("Mongo"), Some("relaxed")),
        cell("u", CellKind::Sql, "SELECT 1", on("Otra"), Some("lejos")),
        cell("k", CellKind::Sql, "SELECT * FROM s EMIT CHANGES;", on("Ksql"), Some("stream")),
        cell("e", CellKind::Sql, "PUT /idx\n{\"settings\": {}}", on("Es"), Some("crea")),
        cell("s", CellKind::Sql, "GET /ventas/_search\n{\"query\": {\"match_all\": {}}}", on("Es"), Some("busca")),
        cell("r", CellKind::Sql, "SET a 1\nGET a", on("Redis"), Some("kv")),
    ];
    let conns = HashMap::from([
        ("m".to_string(), conn("Mongo", cfg("mongodb", "m", 27017, "d", "", &[]))),
        ("k".to_string(), conn("Ksql", cfg("ksqldb", "k", 8088, "", "", &[]))),
        ("e".to_string(), conn("Es", cfg("elasticsearch", "es.example.com", 9200, "", "elastic", &[]))),
        ("s".to_string(), conn("Es", cfg("elasticsearch", "es.example.com", 9200, "", "elastic", &[]))),
        ("r".to_string(), conn("Redis", cfg("redis", "r", 6379, "2", "", &[]))),
    ]);
    let v: Value = serde_json::from_str(&python(&nb, &conns, &PyExport::default())).unwrap();
    let s = sources(&v);
    let find = |id: &str| -> String { v["cells"].as_array().unwrap().iter().position(|c| c["id"] == id).map(|i| s[i].clone()).unwrap() };
    assert!(find("m").starts_with("# TODO: not exported as Python (Mongo): only `db.<collection>.find(…)`"), "{}", find("m"));
    assert!(find("m").ends_with("# db.c.find({activo: true})"));
    assert!(find("u").starts_with("# TODO: this cell runs on «Otra», which isn't set up"), "{}", find("u"));
    assert!(find("k").contains("ksqlDB has no maintained Python client"), "{}", find("k"));
    assert!(find("e").contains("`PUT /idx` changes data"), "{}", find("e"));
    assert_eq!(find("s"), "busca = http_frame(http_es, \"GET\", \"/ventas/_search\", {\"query\": {\"match_all\": {}}})\nbusca");
    assert_eq!(find("r"), "kv = redis_frame(rds_redis, \"\"\"SET a 1\nGET a\"\"\")\nkv");
    assert!(s[1].contains("rds_redis = redis.Redis(host=\"r\", port=6379, db=2, password=os.environ.get(\"DBINE_REDIS_PASSWORD\"), decode_responses=True)"), "{}", s[1]);
    assert!(s[1].contains("def http_frame(") && s[1].contains("def redis_frame("));
}

#[test]
fn mongo_shell_calls_that_map() {
    let MongoCall::Aggregate { coll, pipeline } = mongo_call("// total\ndb.getCollection(\"ventas 2026\").aggregate([{\"$group\": {\"_id\": \"$r\"}}]);").unwrap() else { panic!() };
    assert_eq!(coll, "ventas 2026");
    assert_eq!(pipeline, json!([{"$group": {"_id": "$r"}}]));
    assert!(mongo_call("db.c.find({}).explain()").is_none());
    assert!(mongo_call("db.c.insertOne({\"a\": 1})").is_none());
    assert!(mongo_call("db.c.find({}); db.d.find({})").is_none());
    // Extended JSON goes through bson's json_util.
    let mut imports = BTreeSet::new();
    let a = mongo_arg(&json!({"_id": {"$oid": "65f0c0ffee"}}), &mut imports);
    assert_eq!(a, "json_util.loads(\"{\\\"_id\\\":{\\\"$oid\\\":\\\"65f0c0ffee\\\"}}\")");
    assert!(imports.contains("from bson import json_util"));
}

#[test]
fn python_literals_and_binds() {
    assert_eq!(py_lit("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    assert_eq!(py_block("SELECT \"\"\"x\"\"\" FROM t WHERE a = '\\'"), "\"\"\"SELECT \"\"\\\"x\"\"\\\" FROM t WHERE a = '\\\\'\"\"\"");
    assert_eq!(py_block("SELECT \"a\""), "\"\"\"SELECT \"a\\\"\"\"\"");
    let none = HashSet::new();
    assert_eq!(escape_binds("SELECT x::int, ':a', t.b, '10:30' WHERE y = :y", &none), "SELECT x::int, '\\:a', t.b, '10:30' WHERE y = \\:y");
    assert_eq!(escape_binds("WHERE y = :y AND z = :z", &HashSet::from(["y".to_string()])), "WHERE y = :y AND z = \\:z");
    assert_eq!(slug("Ventas PG (prod) — año"), "ventas_pg_prod_ano");
    assert_eq!(env_var("Ventas PG", "PASSWORD"), "DBINE_VENTAS_PG_PASSWORD");
    assert_eq!(slug("1º"), "c_1");
}

#[test]
fn parameters_go_first_and_are_bound() {
    let mut nb = empty_nb();
    nb.cells = vec![cell("q", CellKind::Sql, "SELECT * FROM v WHERE region = :region AND t::date > '2026-01-01'", on("Ventas PG"), Some("v"))];
    let conns = HashMap::from([("q".to_string(), conn("Ventas PG", cfg("postgres", "pg", 5432, "d", "u", &[])))]);
    let opts = PyExport { parameters: vec![("region".into(), json!("norte"))], ..Default::default() };
    let v: Value = serde_json::from_str(&python(&nb, &conns, &opts)).unwrap();
    let p = &v["cells"][1];
    assert_eq!(p["metadata"]["tags"], json!(["parameters"]));
    assert_eq!(sources(&v)[1], "region = \"norte\"");
    assert!(sources(&v)[3].contains("v = pd.read_sql(text(\"\"\"SELECT * FROM v WHERE region = :region AND t::date > '2026-01-01'\"\"\"), engine_ventas_pg, params={\"region\": region})"), "{}", sources(&v)[3]);
}

/// The notebook's own parameters (`{{name}}`): their defaults in the
/// parameters cell, `:name` binds in SQLAlchemy cells; another client's
/// cell says it doesn't map them.
#[test]
fn notebook_parameters_become_binds() {
    use crate::notebook::params::{NotebookParameter, ParamType};
    let mut nb = empty_nb();
    nb.parameters = vec![
        NotebookParameter { name: "region".into(), kind: ParamType::Text, default: Some(json!("norte")), description: None, required: false, options: vec![] },
        NotebookParameter { name: "tope".into(), kind: ParamType::Number, default: None, description: None, required: true, options: vec![] },
    ];
    nb.cells = vec![
        cell("q", CellKind::Sql, "SELECT * FROM v WHERE region = {{region}} LIMIT {{ tope }}", on("Ventas PG"), Some("v")),
        cell("m", CellKind::Sql, "db.c.find({\"r\": {{region}}})", on("Mongo"), Some("m")),
    ];
    let conns = HashMap::from([
        ("q".to_string(), conn("Ventas PG", cfg("postgres", "pg", 5432, "d", "u", &[]))),
        ("m".to_string(), conn("Mongo", cfg("mongodb", "mongo", 27017, "geo", "u", &[]))),
    ]);
    let opts = PyExport { parameters: parameters(&nb), ..Default::default() };
    let v: Value = serde_json::from_str(&python(&nb, &conns, &opts)).unwrap();
    let s = sources(&v);
    assert_eq!(s[1], "region = \"norte\"\ntope = None");
    let all = s.join("\n");
    assert!(all.contains("WHERE region = :region LIMIT :tope\"\"\"), engine_ventas_pg, params={\"region\": region, \"tope\": tope})"), "{all}");
    assert!(all.contains("only mapped to SQLAlchemy binds"), "{all}");
}

/// A credential's default (or a connection string) is never written into
/// the exported file: `None`, and where to set it from.
#[test]
fn credential_parameter_defaults_are_not_exported() {
    use crate::notebook::params::{NotebookParameter, ParamType};
    let p = |name: &str, default: Value| NotebookParameter { name: name.into(), kind: ParamType::Text, default: Some(default), description: None, required: false, options: vec![] };
    let mut nb = empty_nb();
    nb.parameters = vec![
        p("db_password", json!("hunter2-secret")),
        p("api_token", json!("tok-123-secret")),
        p("origen", json!("Server=db;User Id=sa;Password=pw-secret;")),
        p("region", json!("norte")),
    ];
    nb.cells = vec![cell("q", CellKind::Sql, "SELECT {{db_password}}, {{api_token}}, {{origen}}, {{region}}", on("Ventas PG"), Some("v"))];
    let conns = HashMap::from([("q".to_string(), conn("Ventas PG", cfg("postgres", "pg", 5432, "d", "u", &[])))]);
    let opts = PyExport { parameters: parameters(&nb), ..Default::default() };
    let text = python(&nb, &conns, &opts);
    assert!(!text.contains("secret"), "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let s = sources(&v);
    let lines: Vec<&str> = s[1].lines().collect();
    assert!(lines[0].starts_with("db_password = None  # a credential"), "{}", s[1]);
    assert!(lines[0].ends_with("os.environ[\"DB_PASSWORD\"]"), "{}", s[1]);
    assert!(lines[1].starts_with("api_token = None  #"), "{}", s[1]);
    assert_eq!(lines[2], "origen = None");
    assert_eq!(lines[3], "region = \"norte\"");
}

#[test]
fn dataframe_names_that_clash_are_registered() {
    let mut nb = empty_nb();
    nb.cells = vec![
        cell("a", CellKind::Sql, "SELECT 1 AS x", on("Lite"), Some("pd")),
        cell("b", CellKind::Sql, "SELECT * FROM pd JOIN st_read('mapa.geojson') g ON true", dataframes(), Some("geo")),
    ];
    let conns = HashMap::from([("a".to_string(), conn("Lite", cfg("sqlite", "/tmp/a.db", 0, "", "", &[])))]);
    let v: Value = serde_json::from_str(&python(&nb, &conns, &PyExport::default())).unwrap();
    let s = sources(&v);
    assert!(s[2].starts_with("pd_df = pd.read_sql("), "{}", s[2]);
    assert_eq!(s[3], "duckdb.register(\"pd\", pd_df)\ngeo = duckdb.sql(\"\"\"SELECT * FROM pd JOIN st_read('mapa.geojson') g ON true\"\"\").df()\ngeo");
    assert!(s[1].contains("duckdb.sql(\"INSTALL spatial; LOAD spatial\")"), "{}", s[1]);
}

#[test]
fn plain_charts_become_pandas_plots() {
    let mut nb = empty_nb();
    let mut c = cell("a", CellKind::Sql, "SELECT region, total FROM v", on("Lite"), Some("v"));
    c.chart = Some(json!({ "view": "chart", "charts": [
        { "title": "Por región", "type": "bar", "x": "region", "y": ["total"], "agg": "sum", "sort": "value", "split": null },
        { "title": "", "type": "heatmap", "x": "region", "y": ["total"], "agg": "sum" },
        { "title": "", "type": "line", "x": "region", "y": ["total"], "agg": "none" }
    ]}));
    nb.cells = vec![c];
    let conns = HashMap::from([("a".to_string(), conn("Lite", cfg("sqlite", "/tmp/a.db", 0, "", "", &[])))]);
    let v: Value = serde_json::from_str(&python(&nb, &conns, &PyExport::default())).unwrap();
    let s = sources(&v);
    assert_eq!(
        s[3],
        "v.groupby(\"region\")[[\"total\"]].sum().sort_values(\"total\", ascending=False).plot(kind=\"bar\", title=\"Por región\")\nv.set_index(\"region\")[[\"total\"]].plot(kind=\"line\")"
    );
    assert!(s[0].contains("matplotlib"));
}

/// Writes the multi-engine notebook's SQLite + DuckDB + DataFrame SQL part
/// to `DBINE_PY_EXPORT_OUT` (a path), to run it with Jupyter:
/// `DBINE_PY_EXPORT_OUT=/tmp/x.ipynb DBINE_PY_EXPORT_DIR=/tmp cargo test -p dbine python_export_file -- --ignored`.
#[test]
#[ignore]
fn python_export_file() {
    let out = std::env::var("DBINE_PY_EXPORT_OUT").expect("DBINE_PY_EXPORT_OUT");
    let dir = std::env::var("DBINE_PY_EXPORT_DIR").expect("DBINE_PY_EXPORT_DIR");
    let mut nb = empty_nb();
    nb.cells = vec![
        cell("t", CellKind::Markdown, "# Prueba local", None, None),
        cell("a", CellKind::Sql, "SELECT id, nombre FROM clientes WHERE nombre <> ':x'", on("Lite"), Some("clientes")),
        cell("b", CellKind::Sql, "SELECT cliente, sum(total) AS total FROM ventas GROUP BY cliente", on("Duck"), Some("ventas")),
        cell("c", CellKind::Sql, "SELECT c.nombre, v.total, m.meta\nFROM clientes c\nJOIN ventas v ON v.cliente = c.id\nJOIN read_csv('metas.csv') m ON m.cliente = c.id\nORDER BY c.nombre", dataframes(), Some("cruce")),
        cell("d", CellKind::Sql, "SELECT count(*) AS n FROM cruce", dataframes(), Some("total")),
    ];
    let conns = HashMap::from([
        ("a".to_string(), conn("Lite", cfg("sqlite", &format!("{dir}/crm.db"), 0, "", "", &[]))),
        ("b".to_string(), conn("Duck", cfg("duckdb", &format!("{dir}/ventas.duckdb"), 0, "", "", &[]))),
    ]);
    std::fs::write(out, python(&nb, &conns, &PyExport::default())).unwrap();
}

/// Writes the multi-engine notebook and one with a cell per driver next to
/// `DBINE_PY_EXPORT_OUT`, to validate them with nbformat and compile their
/// cells with Python.
#[test]
#[ignore]
fn python_export_files_for_validation() {
    let out = std::env::var("DBINE_PY_EXPORT_OUT").expect("DBINE_PY_EXPORT_OUT");
    let (nb, conns) = multi_engine();
    std::fs::write(format!("{out}.multi.ipynb"), python(&nb, &conns, &PyExport::default())).unwrap();
    let mut nb = empty_nb();
    let mut conns = HashMap::new();
    for (i, d) in dbine_drivers::all().iter().enumerate() {
        let info = d.info();
        let id = format!("d{i}");
        let src = match info.language {
            dbine_driver::Language::Json if info.id.contains("mongo") || info.id == "documentdb" || info.id == "ferretdb" => "db.c.aggregate([{\"$match\": {\"a\": {\"$gt\": 1}}}])",
            dbine_driver::Language::Json => "GET /x/_search\n{\"query\": {\"match_all\": {}}}",
            dbine_driver::Language::Redis => "GET a",
            _ => "SELECT 1",
        };
        nb.cells.push(cell(&id, CellKind::Sql, src, on(info.id), Some(&format!("q_{}", slug(info.id)))));
        let mut c = cfg(info.id, "db.example.com", 0, "db1", "user1", &[("service", "S1"), ("warehouse", "wh"), ("account", "acct"), ("odbc_driver", "Some Driver"), ("dsn", "MyDsn"), ("project_id", "p1"), ("instance", "i1"), ("region", "eu-west-1")]);
        c.encrypt = i % 2 == 0;
        conns.insert(id, conn(&format!("Conn {}", info.id), c));
    }
    std::fs::write(format!("{out}.all.ipynb"), python(&nb, &conns, &PyExport::default())).unwrap();
}

#[test]
fn write_cells_export_as_a_comment_that_writes_nothing() {
    let (mut nb, conns) = multi_engine();
    let mut w = cell("w", CellKind::Write, "", None, None);
    w.write = Some(crate::notebook::WriteCell {
        source: Some("cruce".into()),
        target: on("CRM MSSQL"),
        schema: Some("dbo".into()),
        table: Some("resumen".into()),
        mode: dbine_driver::WriteMode::Upsert,
        keys: vec!["nombre".into()],
        ..Default::default()
    });
    nb.cells.push(w);
    let v: Value = serde_json::from_str(&python(&nb, &conns, &PyExport::default())).unwrap();
    let all = sources(&v);
    let code = all.iter().find(|s| s.contains("to_sql")).expect("the write cell");
    // Every line is a comment: running the notebook writes nothing.
    assert!(code.lines().all(|l| l.trim().is_empty() || l.trim_start().starts_with('#')), "{code}");
    assert!(code.contains("cruce.to_sql(\"resumen\", engine, schema=\"dbo\", if_exists=\"append\", index=False)"), "{code}");
    assert!(code.contains("staging table"), "{code}");
}
