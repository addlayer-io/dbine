//! Round trips against real servers: create tables on a source engine,
//! read them with its driver, convert, create them on a target engine with
//! the target driver's DDL, and read them back.
//!
//! Servers come from `DBINE_TEST_<ENGINE>_URL` variables
//! (`scheme://user:password@host:port/database`), the same ones the driver
//! crates' integration tests use. A test whose servers aren't set skips.

#![allow(dead_code)]

use dbine_driver::{ConnectionConfig, DdlParts, QueryOutcome, Session, TableSchema};
use dbine_schema::{convert, Conversion, Options};

/// Parse `scheme://user:pass@host:port/db?key=value&…` into a config.
/// Query parameters go to `options` (driver-specific fields).
pub fn config(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap_or(0)));
    let mut cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    };
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, "true"));
        match k {
            "encrypt" => cfg.encrypt = v == "true",
            "trust_server_certificate" => cfg.trust_server_certificate = v == "true",
            _ => {
                cfg.options.insert(k.into(), v.into());
            }
        }
    }
    cfg
}

/// A session on the engine whose URL is in `env`, or `None` to skip.
pub async fn session(driver: &str, env: &str) -> Option<Box<dyn Session>> {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} no está definida: se saltea");
        return None;
    };
    let cfg = config(driver, &url);
    let d = dbine_drivers::find(driver).unwrap_or_else(|| panic!("no driver {driver}"));
    Some(d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("{driver}: {e}")))
}

pub async fn exec(s: &mut Box<dyn Session>, script: &str) {
    let mut out = QueryOutcome::default();
    s.execute(script, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{script}"));
}

/// Run each statement on its own, ignoring errors (cleanup).
pub async fn exec_quiet(s: &mut Box<dyn Session>, statements: &[&str]) {
    for st in statements {
        let mut out = QueryOutcome::default();
        let _ = s.execute(st, 10, &mut out).await;
    }
}

/// The tables named `names` (case-insensitive) as the engine reports them.
pub async fn read(s: &mut Box<dyn Session>, names: &[&str]) -> Vec<TableSchema> {
    let all = s.database_schema().await.expect("database_schema");
    let mut out: Vec<TableSchema> = all.into_iter().filter(|t| names.iter().any(|n| n.eq_ignore_ascii_case(&t.name))).collect();
    out.sort_by_key(|t| names.iter().position(|n| n.eq_ignore_ascii_case(&t.name)));
    out
}

/// The DDL the target driver writes for the converted tables: every
/// table first, then indexes and foreign keys (as the script generator does).
pub fn target_ddl(driver: &str, tables: &[TableSchema]) -> Vec<String> {
    let d = dbine_drivers::find(driver).unwrap();
    let mut out = Vec::new();
    for t in tables {
        let parts = DdlParts { create: true, indexes: true, ..Default::default() };
        out.push(d.table_ddl(t, parts).unwrap_or_else(|e| panic!("table_ddl {}: {e}", t.name)));
    }
    for t in tables.iter().filter(|t| !t.foreign_keys.is_empty()) {
        let parts = DdlParts { foreign_keys: true, ..Default::default() };
        out.push(d.table_ddl(t, parts).unwrap_or_else(|e| panic!("table_ddl fk {}: {e}", t.name)));
    }
    out
}

pub struct RoundTrip {
    pub conversion: Conversion,
    pub ddl: Vec<String>,
    /// The tables as the target reports them after creating them.
    pub back: Vec<TableSchema>,
}

/// Read `names` from `src`, convert, create them on `dst`, read them back.
/// Tables with the target names are dropped first (children before parents:
/// list `names` parents first).
pub async fn round_trip(
    src: &mut Box<dyn Session>,
    src_driver: &str,
    dst: &mut Box<dyn Session>,
    dst_driver: &str,
    names: &[&str],
) -> RoundTrip {
    let tables = read(src, names).await;
    assert_eq!(tables.len(), names.len(), "no se leyeron todas las tablas de origen: {:?}", tables.iter().map(|t| &t.name).collect::<Vec<_>>());
    let conversion = convert(&tables, src_driver, dst_driver, &Options::default()).expect("convert");
    let d = dbine_drivers::find(dst_driver).unwrap();
    for t in conversion.tables.iter().rev() {
        let drop = d.table_ddl(t, DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap_or_default();
        exec_quiet(dst, &[drop.as_str()]).await;
    }
    let ddl = target_ddl(dst_driver, &conversion.tables);
    for script in &ddl {
        exec(dst, script).await;
    }
    let target_names: Vec<&str> = conversion.tables.iter().map(|t| t.name.as_str()).collect();
    let back = read(dst, &target_names).await;
    RoundTrip { conversion, ddl, back }
}

/// Column `c` of table `t` as read back.
pub fn col<'a>(t: &'a TableSchema, c: &str) -> &'a dbine_driver::ColumnDef {
    t.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c)).unwrap_or_else(|| panic!("no column {c} in {}", t.name))
}
