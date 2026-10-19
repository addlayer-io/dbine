use super::*;
use reqwest::Url;

/// A password with every character the formats escape.
const NASTY: &str = "p;w={x}@:/% '\"q";

fn cfg(driver: &str, host: &str, port: u16, db: &str, user: &str) -> ConnectionConfig {
    ConnectionConfig { driver: driver.into(), host: host.into(), port, database: db.into(), username: Some(user.into()), ..Default::default() }
}

fn conn_of(c: &ConnectionConfig) -> Conn {
    let info = dbine_drivers::find(&c.driver).map(|d| d.info());
    Conn::new("Ventas", c, info, "")
}

fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn text(c: &Conn, format: &str, fill: &Fill) -> String {
    build(c, format, fill).unwrap_or_else(|| panic!("{} has no {format}", c.driver)).text
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

// ---------------------------------------------------------------------------
// Escaping.

#[test]
fn escapes_per_format() {
    assert_eq!(pct("a b/@:%"), "a%20b%2F%40%3A%25");
    assert_eq!(decode(&pct(NASTY)), NASTY);
    assert_eq!(ado_val("plain"), "plain");
    assert_eq!(ado_val("a;b"), "\"a;b\"");
    assert_eq!(ado_val("a=b"), "\"a=b\"");
    assert_eq!(ado_val("say \"hi\""), "'say \"hi\"'");
    assert_eq!(ado_val("it's \"x\""), "\"it's \"\"x\"\"\"");
    assert_eq!(ado_val(" pad"), "\" pad\"");
    assert_eq!(odbc_val("plain"), "plain");
    assert_eq!(odbc_val("a;b}c"), "{a;b}}c}");
    assert_eq!(odbc_val("x@y"), "x@y");
    assert_eq!(odbc_val("a=b"), "{a=b}");
    assert_eq!(libpq_val("plain"), "plain");
    assert_eq!(libpq_val("a b'c\\"), "'a b\\'c\\\\'");
    assert_eq!(libpq_val(""), "''");
    assert_eq!(mssql_jdbc_val("a;b}"), "{a;b}}}");
    assert_eq!(prop_val("a=b:c\\ #"), "a\\=b\\:c\\\\ #");
    assert_eq!(prop_val("#x"), "\\#x");
}

// ---------------------------------------------------------------------------
// Placeholders, the mask and the values.

#[test]
fn placeholder_mask_and_password() {
    let c = conn_of(&cfg("postgres", "db.example.com", 5432, "ventas", "app"));
    let ph = text(&c, "uri", &Fill::Placeholder);
    assert_eq!(ph, "postgresql://app:${DB_PASSWORD}@db.example.com:5432/ventas");
    let masked = text(&c, "uri", &Fill::Masked);
    assert!(masked.contains(MASK) && !masked.contains("DB_PASSWORD"), "{masked}");
    let v = values(&[("password", NASTY)]);
    let built = build(&c, "uri", &Fill::Values(&v)).unwrap();
    assert!(built.secret);
    let url = Url::parse(&built.text).unwrap();
    assert_eq!(decode(url.password().unwrap()), NASTY);
    assert_eq!(url.username(), "app");
    assert_eq!(url.host_str(), Some("db.example.com"));
    assert_eq!(url.path(), "/ventas");
    // Without a password to put: no secret, no placeholder.
    let none = BTreeMap::new();
    let bare = build(&c, "uri", &Fill::Values(&none)).unwrap();
    assert!(!bare.secret);
    assert_eq!(bare.text, "postgresql://app@db.example.com:5432/ventas");
    let ph = build(&c, "uri", &Fill::Placeholder).unwrap();
    assert!(!ph.secret);
}

#[test]
fn saved_secret_options_never_show() {
    let mut x = cfg("postgres", "db.example.com", 5432, "ventas", "app");
    x.password = Some("hunter2-pw".into());
    for (k, v) in [("password", "hunter2-opt"), ("token", "hunter2-tok"), ("ssh.password", "hunter2-ssh"), ("ssh.enabled", "true"), ("ssh.host", "bastion"), ("extra", "PWD=hunter2-extra")] {
        x.options.insert(k.into(), v.into());
    }
    let c = conn_of(&x);
    for fill in [Fill::Placeholder, Fill::Masked] {
        for b in build_all(&c, &fill) {
            assert!(!b.text.contains("hunter2"), "{}: {}", b.id, b.text);
        }
    }
}

#[test]
fn every_format_escapes_the_password() {
    let v = values(&[("password", NASTY)]);
    // URIs: the password parses back.
    for (driver, port, format) in [("postgres", 5432, "uri"), ("postgres", 5432, "go"), ("mysql", 3306, "uri"), ("mysql", 3306, "node"), ("sqlserver", 1433, "go"), ("redis", 6379, "uri"), ("mongodb", 27017, "uri"), ("clickhouse", 8123, "uri"), ("postgres", 5432, "sqlalchemy"), ("mysql", 3306, "sqlalchemy"), ("oracle", 1521, "go")] {
        let mut x = cfg(driver, "db.example.com", port, "d", "app");
        x.options.insert("service".into(), "ORCL".into());
        let t = text(&conn_of(&x), format, &Fill::Values(&v));
        let url = Url::parse(&t).unwrap_or_else(|e| panic!("{driver} {format}: {e}: {t}"));
        assert_eq!(decode(url.password().unwrap_or("")), NASTY, "{driver} {format}: {t}");
    }
    let pg = conn_of(&cfg("postgres", "db.example.com", 5432, "d", "app"));
    assert!(text(&pg, "adonet", &Fill::Values(&v)).contains(&format!("Password={};", ado_val(NASTY))));
    assert!(text(&pg, "odbc", &Fill::Values(&v)).contains("PWD={p;w={x}}@:/% '\"q};"));
    assert!(text(&pg, "libpq", &Fill::Values(&v)).contains("password='p;w={x}@:/% \\'\"q'"));
    let jdbc = Url::parse(&text(&pg, "jdbc", &Fill::Values(&v))["jdbc:".len()..]).unwrap();
    assert_eq!(jdbc.query_pairs().find(|(k, _)| k == "password").unwrap().1, NASTY);
    let ms = conn_of(&cfg("sqlserver", "db.example.com", 1433, "d", "app"));
    assert!(text(&ms, "jdbc", &Fill::Values(&v)).contains("password={p;w={x}}@:/% '\"q}"));
    let node: Value = serde_json::from_str(&text(&ms, "node", &Fill::Values(&v))).unwrap();
    assert_eq!(node["password"], NASTY);
    // JDBC with the credentials as properties.
    let db2 = conn_of(&cfg("db2", "db.example.com", 50000, "SAMPLE", "app"));
    let t = text(&db2, "jdbc", &Fill::Values(&v));
    assert_eq!(t, format!("jdbc:db2://db.example.com:50000/SAMPLE\nuser=app\npassword={}", prop_val(NASTY)));
}

// ---------------------------------------------------------------------------
// TLS.

#[test]
fn ssl_mapping() {
    let mut x = cfg("postgres", "h", 5432, "d", "u");
    assert!(!text(&conn_of(&x), "uri", &Fill::Placeholder).contains("sslmode"));
    x.encrypt = true;
    let c = conn_of(&x);
    assert!(text(&c, "uri", &Fill::Placeholder).ends_with("?sslmode=verify-full"));
    assert!(text(&c, "adonet", &Fill::Placeholder).contains("SSL Mode=VerifyFull;"));
    assert!(text(&c, "sqlalchemy", &Fill::Placeholder).contains("sslmode=verify-full"));
    x.trust_server_certificate = true;
    let c = conn_of(&x);
    assert!(text(&c, "uri", &Fill::Placeholder).ends_with("?sslmode=require"));
    assert!(text(&c, "libpq", &Fill::Placeholder).contains("sslmode=require"));

    let mut m = cfg("mysql", "h", 3306, "d", "u");
    m.encrypt = true;
    let c = conn_of(&m);
    assert!(text(&c, "jdbc", &Fill::Placeholder).contains("sslMode=VERIFY_IDENTITY"));
    assert!(text(&c, "go", &Fill::Placeholder).contains("tls=true"));
    assert!(text(&c, "adonet", &Fill::Placeholder).contains("SslMode=VerifyFull"));
    m.trust_server_certificate = true;
    assert!(text(&conn_of(&m), "go", &Fill::Placeholder).contains("tls=skip-verify"));
    let mut mdb = cfg("mariadb", "h", 3306, "d", "u");
    mdb.encrypt = true;
    assert!(text(&conn_of(&mdb), "jdbc", &Fill::Placeholder).starts_with("jdbc:mariadb://h:3306/d?user=u&password=${DB_PASSWORD}&sslMode=verify-full"));

    let ms = conn_of(&cfg("sqlserver", "h", 1433, "d", "u"));
    let ado = text(&ms, "adonet", &Fill::Placeholder);
    assert!(ado.contains("Encrypt=False;TrustServerCertificate=True;"), "{ado}");
    let az = conn_of(&cfg("azuresql", "x.database.windows.net", 1433, "d", "u"));
    let ado = text(&az, "adonet", &Fill::Placeholder);
    assert!(ado.contains("Encrypt=True;") && !ado.contains("TrustServerCertificate"), "{ado}");
    assert!(text(&az, "odbc", &Fill::Placeholder).contains("Encrypt=yes;"));
    assert!(text(&az, "go", &Fill::Placeholder).contains("encrypt=true"));
}

// ---------------------------------------------------------------------------
// Sign-in methods.

#[test]
fn sql_server_sign_in() {
    let mut x = cfg("sqlserver", "srv\\SQLEXPRESS", 1433, "d", "");
    x.options.insert("auth".into(), "windows_integrated".into());
    let c = conn_of(&x);
    let ado = text(&c, "adonet", &Fill::Placeholder);
    assert!(ado.starts_with("Server=srv\\SQLEXPRESS;Database=d;Integrated Security=True;"), "{ado}");
    assert!(!ado.contains("Password"));
    assert!(text(&c, "jdbc", &Fill::Placeholder).starts_with("jdbc:sqlserver://srv;instanceName=SQLEXPRESS;databaseName=d;"));
    assert!(build(&c, "sqlalchemy", &Fill::Placeholder).is_none());

    let mut sp = cfg("azuresql", "x.database.windows.net", 1433, "d", "");
    sp.options.insert("auth".into(), "entra_sp".into());
    sp.options.insert("client_id".into(), "11111111-2222".into());
    sp.options.insert("tenant_id".into(), "tttt".into());
    let c = conn_of(&sp);
    let v = values(&[("client_secret", "s;cret")]);
    let ado = text(&c, "adonet", &Fill::Values(&v));
    assert!(ado.contains("Authentication=Active Directory Service Principal;User ID=11111111-2222;Password=\"s;cret\";"), "{ado}");
    assert!(text(&c, "odbc", &Fill::Placeholder).contains("Authentication=ActiveDirectoryServicePrincipal;UID=11111111-2222;PWD=${DB_CLIENT_SECRET};"));
    let node: Value = serde_json::from_str(&text(&c, "node", &Fill::Values(&v))).unwrap();
    assert_eq!(node["authentication"]["options"]["clientSecret"], "s;cret");
    assert!(text(&c, "go", &Fill::Placeholder).contains("fedauth=ActiveDirectoryServicePrincipal"));
}

#[test]
fn iam_token_is_never_a_password() {
    let mut x = cfg("postgres", "x.rds.amazonaws.com", 5432, "d", "app");
    x.options.insert("auth".into(), "aws_iam".into());
    let c = conn_of(&x);
    assert!(c.iam);
    let v = values(&[("password", "not-this")]);
    for b in build_all(&c, &Fill::Values(&v)) {
        assert!(!b.text.contains("not-this"), "{}: {}", b.id, b.text);
    }
    assert!(text(&c, "uri", &Fill::Values(&v)).contains("${DB_IAM_TOKEN}"));
    let dsql = conn_of(&cfg("dsql", "abc.dsql.us-east-1.on.aws", 0, "", ""));
    assert_eq!(text(&dsql, "uri", &Fill::Placeholder), "postgresql://admin:${DB_IAM_TOKEN}@abc.dsql.us-east-1.on.aws:5432/postgres?sslmode=verify-full");
}

// ---------------------------------------------------------------------------
// Tunnels: the string points to the database server.

#[test]
fn tunnel_uses_the_real_host() {
    let mut x = cfg("mysql", "db.internal", 3306, "d", "u");
    x.options.insert("ssh.enabled".into(), "true".into());
    x.options.insert("ssh.host".into(), "bastion.example.com".into());
    let c = conn_of(&x);
    for b in build_all(&c, &Fill::Placeholder) {
        assert!(b.text.contains("db.internal") && !b.text.contains("bastion") && !b.text.contains("127.0.0.1"), "{}: {}", b.id, b.text);
    }
}

// ---------------------------------------------------------------------------
// Samples.

#[test]
fn samples() {
    let ph = &Fill::Placeholder;
    let pg = conn_of(&cfg("postgres", "h", 5432, "d", "u"));
    assert_eq!(text(&pg, "libpq", ph), "host=h port=5432 dbname=d user=u password=${DB_PASSWORD}");
    assert_eq!(text(&pg, "adonet", ph), "Host=h;Port=5432;Database=d;Username=u;Password=${DB_PASSWORD};");
    assert_eq!(text(&pg, "jdbc", ph), "jdbc:postgresql://h:5432/d?user=u&password=${DB_PASSWORD}");
    assert_eq!(text(&pg, "sqlalchemy", ph), "postgresql+psycopg://u:${DB_PASSWORD}@h:5432/d");
    assert_eq!(text(&pg, "odbc", ph), "DRIVER={PostgreSQL Unicode};SERVER=h;PORT=5432;DATABASE=d;UID=u;PWD=${DB_PASSWORD};");
    let rs = conn_of(&cfg("redshift", "r.example.com", 0, "dev", "u"));
    assert!(text(&rs, "jdbc", ph).starts_with("jdbc:redshift://r.example.com:5439/dev?"));

    let my = conn_of(&cfg("mysql", "h", 3306, "d", "u"));
    assert_eq!(text(&my, "go", ph), "u:${DB_PASSWORD}@tcp(h:3306)/d?parseTime=true");
    assert_eq!(text(&my, "adonet", ph), "Server=h;Port=3306;Database=d;User ID=u;Password=${DB_PASSWORD};");

    let ms = conn_of(&cfg("sqlserver", "h", 1433, "d", "sa"));
    assert_eq!(text(&ms, "adonet", ph), "Server=tcp:h,1433;Database=d;User ID=sa;Password=${DB_PASSWORD};Encrypt=False;TrustServerCertificate=True;");
    assert_eq!(text(&ms, "go", ph), "sqlserver://sa:${DB_PASSWORD}@h:1433?database=d&encrypt=disable");
    assert_eq!(text(&ms, "sqlalchemy", ph), "mssql+pymssql://sa:${DB_PASSWORD}@h:1433/d");

    let mut ora = cfg("oracle", "h", 1521, "", "scott");
    ora.options.insert("service".into(), "FREEPDB1".into());
    let ora = conn_of(&ora);
    assert_eq!(text(&ora, "easyconnect", ph), "h:1521/FREEPDB1");
    assert_eq!(text(&ora, "jdbc", ph), "jdbc:oracle:thin:@//h:1521/FREEPDB1\nuser=scott\npassword=${DB_PASSWORD}");
    assert_eq!(text(&ora, "oracledb", ph), "scott/${DB_PASSWORD}@h:1521/FREEPDB1");
    let v = values(&[("password", "a/b")]);
    assert_eq!(text(&ora, "oracledb", &Fill::Values(&v)), "scott/\"a/b\"@h:1521/FREEPDB1");
    assert_eq!(text(&ora, "adonet", ph), "User Id=scott;Password=${DB_PASSWORD};Data Source=h:1521/FREEPDB1;");

    let lite = conn_of(&cfg("sqlite", "/data/my db.sqlite", 0, "", ""));
    assert_eq!(text(&lite, "uri", ph), "file:/data/my%20db.sqlite");
    assert_eq!(text(&lite, "adonet", ph), "Data Source=/data/my db.sqlite;");
    assert_eq!(text(&lite, "sqlalchemy", ph), "sqlite:////data/my db.sqlite");

    let mut mongo = cfg("mongodb", "m1.example.com", 27017, "app", "u");
    mongo.options.insert("auth_source".into(), "admin".into());
    mongo.encrypt = true;
    let mongo = conn_of(&mongo);
    assert_eq!(text(&mongo, "uri", ph), "mongodb://u:${DB_PASSWORD}@m1.example.com:27017/app?authSource=admin&tls=true");
    let srv = conn_of(&cfg("mongodb", "mongodb+srv://cluster0.abc.mongodb.net/?retryWrites=true", 0, "app", "u"));
    // The host's own options are left out (they may hold credentials).
    assert_eq!(text(&srv, "uri", ph), "mongodb+srv://u:${DB_PASSWORD}@cluster0.abc.mongodb.net/app");
    let saved = values(&[("connection_string", "mongodb://x:y@z/")]);
    assert_eq!(text(&srv, "uri", &Fill::Values(&saved)), "mongodb://x:y@z/");

    let mut redis = cfg("redis", "r", 6380, "2", "");
    redis.encrypt = true;
    assert_eq!(text(&conn_of(&redis), "uri", ph), "rediss://:${DB_PASSWORD}@r:6380/2");

    let mut sf = cfg("snowflake", "", 0, "DB", "u");
    sf.options.insert("account".into(), "xy12345".into());
    sf.options.insert("warehouse".into(), "WH".into());
    let sf = conn_of(&sf);
    assert_eq!(text(&sf, "go", ph), "u:${DB_TOKEN}@xy12345/DB?warehouse=WH");
    assert!(text(&sf, "jdbc", ph).starts_with("jdbc:snowflake://xy12345.snowflakecomputing.com/?db=DB&warehouse=WH\nuser=u\npassword=${DB_TOKEN}"));

    let ch = conn_of(&cfg("clickhouse", "c", 0, "default", "u"));
    assert_eq!(text(&ch, "uri", ph), "http://u:${DB_PASSWORD}@c:8123/?database=default");

    let cos = conn_of(&cfg("cosmosdb", "https://acct.documents.azure.com:443/", 0, "db", ""));
    assert_eq!(text(&cos, "azure", ph), "AccountEndpoint=https://acct.documents.azure.com:443/;AccountKey=${DB_ACCOUNT_KEY};");

    let mut odbc = cfg("odbc", "", 0, "", "u");
    odbc.options.insert("dsn".into(), "Sales DSN".into());
    assert_eq!(text(&conn_of(&odbc), "odbc", ph), "DSN=Sales DSN;UID=u;PWD=${DB_PASSWORD};");
    let raw = conn_of(&cfg("odbc", "", 0, "", ""));
    assert_eq!(text(&raw, "odbc", ph), "${DB_CONNECTION_STRING}");
    assert_eq!(text(&raw, "odbc", &Fill::Values(&values(&[("connection_string", "Driver={X};PWD=1;")]))), "Driver={X};PWD=1;");
}

/// Every engine gets at least one format, and docs/connection-strings.md
/// lists each one by name.
#[test]
fn every_engine_has_a_format_and_is_documented() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/connection-strings.md")).unwrap();
    let doc = doc.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut missing = Vec::new();
    let mut undocumented = Vec::new();
    for info in dbine_drivers::infos() {
        let mut x = cfg(info.id, "db.example.com", 0, "d", "u");
        for (k, v) in [("account", "acct"), ("project_id", "p"), ("instance", "i"), ("service", "S"), ("odbc_driver", "Some Driver"), ("dsn", "D"), ("region", "us-east-1"), ("warehouse", "w")] {
            x.options.insert(k.into(), v.into());
        }
        let c = conn_of(&x);
        if build_all(&c, &Fill::Placeholder).is_empty() {
            missing.push(info.id);
        }
        if !doc.contains(&format!("| {} |", info.name)) {
            undocumented.push(info.name);
        }
    }
    assert!(missing.is_empty(), "engines without a format: {missing:?}");
    assert!(undocumented.is_empty(), "engines not in docs/connection-strings.md: {undocumented:?}");
}

