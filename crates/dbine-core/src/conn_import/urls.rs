//! Pasted connections, one per line:
//!
//! - URLs: `postgres://…`, `mysql://…`, `mongodb+srv://…`, `redis://…`,
//!   `sqlserver://…`, `sqlite:///ruta`…;
//! - JDBC URLs: `jdbc:postgresql://…`, `jdbc:oracle:thin:@…`;
//! - SQL Server connection strings: `Server=…;Database=…;User Id=…`;
//! - a path to a SQLite or DuckDB file.

use super::{apply_ado, apply_jdbc, libsql_url, parse_url, redact_url, Candidate, Found};
use std::path::PathBuf;

pub fn read(text: &str) -> Found {
    let candidates = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .enumerate()
        .map(|(n, l)| parse(n, l))
        .collect();
    Found { path: PathBuf::new(), candidates, warnings: Vec::new() }
}

/// What the line looks like, for the "engine" column of what can't be read.
fn kind_of(line: &str) -> String {
    line.split("://").next().filter(|s| s.len() < line.len()).unwrap_or("texto").to_string()
}

fn parse(n: usize, line: &str) -> Candidate {
    let mut c = Candidate::new(n.to_string(), String::new(), kind_of(line));
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("jdbc:") {
        if c.set_driver(&lower) && !apply_jdbc(&mut c, line) {
            c.unsupported = Some("no se pudo leer la URL JDBC".into());
        }
    } else if let Some((scheme, _)) = lower.split_once("://") {
        let driver = match scheme {
            "postgres" | "postgresql" => "postgres",
            "mongodb" | "mongodb+srv" => "mongodb",
            "redis" | "rediss" => "redis",
            "sqlserver" | "mssql" => "sqlserver",
            "neo4j" | "neo4j+s" | "bolt" | "bolt+s" => "neo4j",
            "libsql" => "libsql",
            "sqlite" => "sqlite",
            "duckdb" => "duckdb",
            "http" | "https" => {
                c.unsupported = Some("una URL http no dice qué motor es: creala a mano eligiendo el motor".into());
                ""
            }
            other => match super::driver_for(other) {
                Ok(d) => d,
                Err(why) => {
                    c.unsupported = Some(why);
                    ""
                }
            },
        };
        c.config.driver = driver.to_string();
        match driver {
            "" => {}
            "sqlite" | "duckdb" => c.config.host = line.split_once("://").map(|(_, p)| p).unwrap_or("").to_string(),
            "libsql" => {
                // `?authToken=…` goes to the secret option, not the host.
                let (base, token) = libsql_url(line);
                c.config.host = base;
                if let Some(t) = token {
                    c.config.options.insert("auth_token".into(), t);
                }
            }
            "mongodb" => {
                // The whole string goes along: options, replica set, SRV.
                c.config.options.insert("connection_string".into(), line.to_string());
                if let Some(u) = parse_url(line) {
                    c.config.host = u.host;
                    c.config.port = u.port;
                    c.config.database = u.path;
                    c.config.username = u.user;
                }
            }
            _ => {
                apply_jdbc(&mut c, line);
                if scheme.ends_with('s') && matches!(scheme, "rediss" | "neo4j+s" | "bolt+s") {
                    c.config.encrypt = true;
                }
            }
        }
    } else if line.contains('=') && line.contains(';') {
        c.source_kind = "cadena de conexión".into();
        c.config.driver = "sqlserver".into();
        apply_ado(&mut c, line);
        if c.config.host.is_empty() {
            c.unsupported = Some("la cadena de conexión no tiene Server ni Data Source".into());
        }
    } else {
        let l = lower.trim_matches(['"', '\'']);
        let driver = if l.ends_with(".duckdb") || l.ends_with(".ddb") {
            "duckdb"
        } else if [".db", ".sqlite", ".sqlite3", ".db3"].iter().any(|e| l.ends_with(e)) {
            "sqlite"
        } else {
            ""
        };
        if driver.is_empty() {
            c.unsupported = Some("no parece una URL ni una cadena de conexión".into());
        } else {
            c.config.driver = driver.into();
            c.config.host = line.trim_matches(['"', '\'']).to_string();
            c.source_kind = "archivo".into();
        }
    }
    c.name = name_of(&c, line);
    c
}

fn name_of(c: &Candidate, line: &str) -> String {
    let cfg = &c.config;
    if matches!(cfg.driver.as_str(), "sqlite" | "duckdb") {
        return std::path::Path::new(&cfg.host).file_name().and_then(|f| f.to_str()).unwrap_or(&cfg.host).to_string();
    }
    if let Some(name) = cfg.options.get("connect_descriptor").and_then(|d| super::descriptor_name(d)) {
        return name;
    }
    let host = cfg.host.split([',', '\\']).next().unwrap_or("").to_string();
    match (host.is_empty(), cfg.database.is_empty()) {
        (false, false) => format!("{host} / {}", cfg.database),
        (false, true) => host,
        _ => safe_line(line).chars().take(40).collect(),
    }
}

/// The pasted line without the secrets it may carry: a URL's login and
/// query string, a connection string's password.
fn safe_line(line: &str) -> String {
    let line = if line.contains("://") { redact_url(line) } else { line.to_string() };
    // `jdbc:oracle:thin:user/password@…` and EZConnect `user/password@…`.
    if let Some((head, target)) = line.rsplit_once('@') {
        if head.contains('/') || head.to_ascii_lowercase().starts_with("jdbc:oracle") {
            return format!("@{}", target.trim());
        }
    }
    line.split(';')
        .filter(|p| {
            let k = p.split_once('=').map(|(k, _)| k).unwrap_or("").trim().to_ascii_lowercase().replace(' ', "");
            !matches!(k.as_str(), "password" | "pwd")
        })
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_pasted_lines() {
        let f = read(
            "postgres://ana:pw@pg.local:5433/app?sslmode=require\n\
             # a comment\n\
             mongodb+srv://u:p@cluster0.example.net/tienda?retryWrites=true\n\
             Server=tcp:srv.database.windows.net,1433;Database=ventas;User Id=sa;Password=x;Encrypt=True\n\
             jdbc:oracle:thin:@//ora:1521/FREEPDB1\n\
             rediss://:s@cache:6380/1\n\
             /Users/ana/datos.sqlite\n\
             https://algo\n\
             cualquier cosa",
        );
        let c = &f.candidates;
        assert_eq!(c.len(), 8);
        assert_eq!((c[0].config.driver.as_str(), c[0].config.host.as_str(), c[0].config.port, c[0].config.password.as_deref(), c[0].config.encrypt), ("postgres", "pg.local", 5433, Some("pw"), true));
        assert_eq!(c[0].name, "pg.local / app");
        assert_eq!((c[1].config.driver.as_str(), c[1].config.database.as_str()), ("mongodb", "tienda"));
        assert!(c[1].config.options.contains_key("connection_string"));
        assert_eq!((c[2].config.driver.as_str(), c[2].config.host.as_str(), c[2].config.password.as_deref()), ("sqlserver", "srv.database.windows.net,1433", Some("x")));
        assert_eq!((c[3].config.driver.as_str(), c[3].config.options["service"].as_str()), ("oracle", "FREEPDB1"));
        assert_eq!((c[4].config.driver.as_str(), c[4].config.encrypt, c[4].config.database.as_str()), ("redis", true, "1"));
        assert_eq!((c[5].config.driver.as_str(), c[5].name.as_str()), ("sqlite", "datos.sqlite"));
        assert!(c[6].unsupported.is_some() && c[7].unsupported.is_some());
    }

    #[test]
    fn libsql_tokens_go_to_the_secret_option() {
        let f = read("libsql://app-org.turso.io?authToken=eyJhbGciOi.payload.sig\nlibsql://localhost:8080?auth_token=t0k&x=1");
        let c = &f.candidates;
        assert_eq!((c[0].config.driver.as_str(), c[0].config.host.as_str()), ("libsql", "libsql://app-org.turso.io"));
        assert_eq!(c[0].config.options.get("auth_token").map(String::as_str), Some("eyJhbGciOi.payload.sig"));
        assert_eq!(c[0].name, "libsql://app-org.turso.io");
        assert!(c[0].has_secret());
        assert_eq!((c[1].config.host.as_str(), c[1].config.options.get("auth_token").map(String::as_str)), ("libsql://localhost:8080", Some("t0k")));
        for c in c {
            assert!(!c.name.contains("t0k") && !c.name.contains("eyJ") && !c.config.host.contains('?'), "{}", c.name);
        }
    }

    #[test]
    fn names_never_carry_a_password() {
        let f = read("postgres://ana:s3cret@/app\nhttps://u:s3cret@algo/x?token=s3cret\nfoo://u:s3cret@h\nDatabase=x;Password=s3cret;User Id=sa");
        for c in &f.candidates {
            assert!(!c.name.contains("s3cret"), "{}", c.name);
        }
    }

    #[test]
    fn an_oracle_descriptor_url_keeps_its_login_out_of_the_name() {
        let f = read("jdbc:oracle:thin:scott/S3cretPw@(DESCRIPTION=(ADDRESS=(HOST=ora)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=XE)))\njdbc:oracle:thin:scott/S3cretPw@//ora:1521/XE");
        for c in &f.candidates {
            assert!(!c.name.contains("S3cretPw"), "{}", c.name);
            assert!(!serde_json::to_string(&c.config.options).unwrap().contains("S3cretPw"));
            assert_eq!(c.config.username.as_deref(), Some("scott"));
            assert_eq!(c.config.password.as_deref(), Some("S3cretPw"));
        }
        assert_eq!(f.candidates[0].name, "ora / XE");
        // A JDBC URL with no host before its `;` properties.
        let f = read("jdbc:sqlserver://;user=sa;password=Pr0dPw;serverName=sql1\nsqlserver://;pwd=Pr0dPw;server=h");
        for c in &f.candidates {
            assert!(!c.name.contains("Pr0dPw"), "{}", c.name);
        }
        // A password with `@`, quoted or not, stays whole and out of the target.
        let f = read("jdbc:oracle:thin:scott/\"Pa@ssw0rd\"@//ora:1521/XE\njdbc:oracle:thin:scott/Pa@ss@(DESCRIPTION=(ADDRESS=(HOST=ora))(CONNECT_DATA=(SID=X)))");
        assert_eq!(f.candidates[0].config.password.as_deref(), Some("Pa@ssw0rd"));
        assert_eq!(f.candidates[1].config.password.as_deref(), Some("Pa@ss"));
        for c in &f.candidates {
            assert!(!c.name.contains("ss"), "{}", c.name);
            assert!(!c.config.host.contains('@') && !serde_json::to_string(&c.config.options).unwrap().contains("Pa@"));
        }
    }
}
