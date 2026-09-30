//! Building ODBC connection strings from the connection form.

use crate::presets::{Preset, V};
use dbine_driver::{ConnectionConfig, Error, Result};

/// An attribute value, braced when it holds `;`, `{`, `}` or `=` or has
/// surrounding spaces (`}` doubles inside braces, as ODBC specifies).
pub fn escape(v: &str) -> String {
    let needs = v.contains([';', '{', '}', '=']) || v.starts_with(' ') || v.ends_with(' ');
    if needs {
        format!("{{{}}}", v.replace('}', "}}"))
    } else {
        v.to_string()
    }
}

/// `DRIVER={name}`: always braced (names have spaces); braces the user
/// typed around it are dropped first.
pub fn driver_value(name: &str) -> String {
    let n = name.trim();
    let n = n.strip_prefix('{').and_then(|s| s.strip_suffix('}')).unwrap_or(n);
    format!("{{{}}}", n.replace('}', "}}"))
}

fn has_key(conn_str: &str, key: &str) -> bool {
    conn_str.split(';').any(|kv| kv.split('=').next().is_some_and(|k| k.trim().eq_ignore_ascii_case(key)))
}

/// The connection string for `preset`, on `database` (may be empty).
pub fn build(preset: &Preset, cfg: &ConnectionConfig, database: &str) -> Result<String> {
    let user = cfg.username_or_empty();
    let password = cfg.password_or_empty();
    if preset.is_generic() {
        let mut s = if let Some(cs) = cfg.option("connection_string") {
            cs.trim().trim_end_matches(';').to_string()
        } else if let Some(dsn) = cfg.option("dsn") {
            format!("DSN={}", escape(dsn.trim()))
        } else {
            return Err(Error::Connect("Indicá un DSN o una cadena de conexión ODBC.".into()));
        };
        if !user.is_empty() && !has_key(&s, "UID") {
            s.push_str(&format!(";UID={}", escape(user)));
        }
        if !password.is_empty() && !has_key(&s, "PWD") {
            s.push_str(&format!(";PWD={}", escape(password)));
        }
        return Ok(s);
    }

    let driver = cfg.option("odbc_driver").unwrap_or(preset.driver_hint);
    let port = cfg.port_or(preset.default_port).to_string();
    let host = cfg.host.trim();
    let mut parts: Vec<String> = Vec::new();
    for (key, v) in preset.template {
        let value = match v {
            V::Driver => {
                parts.push(format!("{key}={}", driver_value(driver)));
                continue;
            }
            V::Host => host.to_string(),
            V::Port => port.clone(),
            V::HostPort => {
                if host.is_empty() {
                    String::new()
                } else {
                    format!("{host}:{port}")
                }
            }
            V::Database => database.to_string(),
            V::User => user.to_string(),
            V::Password => password.to_string(),
            V::Opt(k) => cfg.option(k).unwrap_or("").to_string(),
            V::Lit(l) => l.to_string(),
            V::FreeTdsVersion => {
                if driver.to_ascii_lowercase().contains("freetds") {
                    "5.0".into()
                } else {
                    String::new()
                }
            }
            V::SimbaAuthMech => match (user.is_empty(), password.is_empty()) {
                (false, false) => "3".into(),
                (false, true) => "2".into(),
                _ => "0".into(),
            },
            V::OptOr(k, default) => cfg.option(k).map(str::trim).filter(|v| !v.is_empty()).unwrap_or(default).to_string(),
            V::Fmt(f) => {
                // Written as is: the format decides the separators.
                let value = fill(f, host, &port, database, user, |k| cfg.option(k).unwrap_or("").trim().to_string());
                if !value.is_empty() {
                    parts.push(format!("{key}={value}"));
                }
                continue;
            }
        };
        if !value.is_empty() {
            parts.push(format!("{key}={}", escape(&value)));
        }
    }
    if let Some(extra) = cfg.option("extra") {
        let extra = extra.trim().trim_matches(';');
        if !extra.is_empty() {
            parts.push(extra.to_string());
        }
    }
    Ok(parts.join(";"))
}

/// A [`V::Fmt`] value: `{host}`, `{port}`, `{database}`, `{user}`,
/// `{opt:key}`. Empty when every placeholder it uses is empty.
fn fill(f: &str, host: &str, port: &str, database: &str, user: &str, opt: impl Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut any = false;
    let mut rest = f;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else { break };
        let name = &rest[start + 1..start + end];
        let v = match name {
            "host" => host.to_string(),
            "port" => port.to_string(),
            "database" => database.to_string(),
            "user" => user.to_string(),
            n => n.strip_prefix("opt:").map(&opt).unwrap_or_default(),
        };
        // The port always has a value (the default): it doesn't count.
        any |= !v.is_empty() && name != "port";
        out.push_str(&v);
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    if any {
        out
    } else {
        String::new()
    }
}

/// Escapes `_` and `%` (and the escape itself) in a catalog-function
/// pattern argument.
pub fn escape_pattern(s: &str, esc: &str) -> String {
    if esc.is_empty() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 4);
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if rest.starts_with(esc) {
            out.push_str(esc);
            out.push_str(esc);
            rest = &rest[esc.len()..];
            continue;
        }
        if c == '_' || c == '%' {
            out.push_str(esc);
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn cfg(pairs: &[(&str, &str)]) -> ConnectionConfig {
        ConnectionConfig {
            host: "db.local".into(),
            username: Some("app".into()),
            password: Some("s3cr;t}x".into()),
            options: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn values_with_separators_are_braced() {
        assert_eq!(escape("plain"), "plain");
        assert_eq!(escape("a;b"), "{a;b}");
        assert_eq!(escape("a}b"), "{a}}b}");
        assert_eq!(escape("{x}"), "{{x}}}");
        assert_eq!(escape(" pad"), "{ pad}");
        assert_eq!(driver_value("{IBM DB2 ODBC DRIVER}"), "{IBM DB2 ODBC DRIVER}");
        assert_eq!(driver_value("FreeTDS"), "{FreeTDS}");
    }

    #[test]
    fn db2_template() {
        let s = build(preset("db2"), &cfg(&[]), "SAMPLE").unwrap();
        assert_eq!(
            s,
            "DRIVER={IBM DB2 ODBC DRIVER};DATABASE=SAMPLE;HOSTNAME=db.local;PORT=50000;PROTOCOL=TCPIP;UID=app;PWD={s3cr;t}}x}"
        );
    }

    #[test]
    fn empty_values_are_left_out_and_extras_appended() {
        let mut c = cfg(&[("odbc_driver", "Vertica"), ("extra", ";SSLMode=require;")]);
        c.password = None;
        c.port = 6000;
        let s = build(preset("vertica"), &c, "").unwrap();
        assert_eq!(s, "DRIVER={Vertica};ServerName=db.local;Port=6000;UID=app;SSLMode=require");
    }

    #[test]
    fn freetds_gets_tds_5() {
        let s = build(preset("sybase"), &cfg(&[]), "pubs2").unwrap();
        assert!(s.ends_with(";TDS_Version=5.0"), "{s}");
        let s = build(preset("sybase"), &cfg(&[("odbc_driver", "Adaptive Server Enterprise")]), "").unwrap();
        assert!(!s.contains("TDS_Version"), "{s}");
    }

    #[test]
    fn host_port_and_auth_mech() {
        let s = build(preset("sqlanywhere"), &cfg(&[]), "demo").unwrap();
        assert!(s.contains("HOST=db.local:2638;DBN=demo"), "{s}");
        let mut c = cfg(&[]);
        c.password = None;
        let s = build(preset("hive"), &c, "").unwrap();
        assert!(s.contains("AuthMech=2"), "{s}");
    }

    #[test]
    fn generic_takes_dsn_or_string() {
        let g = preset("odbc");
        assert!(build(g, &ConnectionConfig::default(), "").is_err());
        let s = build(g, &cfg(&[("dsn", "Sales")]), "").unwrap();
        assert_eq!(s, "DSN=Sales;UID=app;PWD={s3cr;t}}x}");
        let s = build(g, &cfg(&[("connection_string", "DRIVER={X};uid=me;PWD=p;")]), "").unwrap();
        assert_eq!(s, "DRIVER={X};uid=me;PWD=p");
    }

    #[test]
    fn formatted_values_and_defaults() {
        let s = build(preset("ingres"), &cfg(&[]), "demodb").unwrap();
        assert!(s.contains(";SERVER=@db.local,tcp_ip,21064;SERVERTYPE=INGRES;DATABASE=demodb;"), "{s}");
        let s = build(preset("nuodb"), &cfg(&[("schema", "HR")]), "test").unwrap();
        assert!(s.contains(";DATABASE=test@db.local:48004;UID=app;"), "{s}");
        assert!(s.ends_with(";SCHEMA=HR"), "{s}");
        let s = build(preset("zen"), &cfg(&[]), "DEMODATA").unwrap();
        assert!(s.contains("ServerName=db.local.1583;DBQ=DEMODATA"), "{s}");
        let s = build(preset("sqream"), &cfg(&[("cluster", "true")]), "master").unwrap();
        assert!(s.ends_with(";Service=sqream;Cluster=true"), "{s}");
        let s = build(preset("netsuite"), &cfg(&[("account_id", "123"), ("role_id", "3")]), "").unwrap();
        assert!(s.contains(";Port=1708;Encrypted=1;AllowSinglePacketLogout=1;Truststore=system;ServerDataSource=NetSuite2.com;"), "{s}");
        assert!(s.ends_with(";CustomProperties=AccountID=123;RoleID=3"), "{s}");
        let s = build(preset("cloudera"), &cfg(&[]), "").unwrap();
        assert!(s.contains("ThriftTransport=2;HTTPPath=cliservice;SSL=1"), "{s}");
        // A format whose placeholders are all empty is left out.
        let mut c = cfg(&[]);
        c.host = String::new();
        let s = build(preset("ingres"), &c, "db").unwrap();
        assert!(!s.contains("SERVER="), "{s}");
    }

    #[test]
    fn file_presets_take_the_path() {
        let mut c = cfg(&[]);
        c.password = None;
        let s = build(preset("access"), &c, r"C:\datos\ventas.accdb").unwrap();
        assert_eq!(s, r"DRIVER={Microsoft Access Driver (*.mdb, *.accdb)};DBQ=C:\datos\ventas.accdb");
        let s = build(preset("dbase"), &c, r"C:\dbf").unwrap();
        assert!(s.ends_with(r"DBQ=C:\dbf;DriverID=277"), "{s}");
    }

    #[test]
    fn patterns_escape_wildcards() {
        assert_eq!(escape_pattern("my_table%", "\\"), "my\\_table\\%");
        assert_eq!(escape_pattern("a\\b", "\\"), "a\\\\b");
        assert_eq!(escape_pattern("x_y", ""), "x_y");
    }
}
