//! "Calidad de código": the query editor's linter. Pure text analysis, no
//! round trip to the database: each engine's language gets a tokenizer that
//! knows its strings, comments and quoted names (so nothing inside them is
//! ever reported) and the rules that make sense for it. The UI owns the
//! texts (i18n `lint:rules.<id>`): a finding carries its rule, its byte
//! range and the values its message shows.

mod cql;
mod cypher;
mod lex;
mod mongo;
mod console;
mod shell;
mod sql;

use dbine_driver::sql::ScriptDialect;
use dbine_driver::{DriverInfo, Language};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// The languages and engine families rules are written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    /// Every SQL engine.
    Sql,
    /// SQL Server, Azure SQL, Fabric, Sybase ASE, SQL Anywhere.
    Tsql,
    Postgres,
    Mysql,
    Oracle,
    /// InfluxQL (InfluxDB 1.x) and InfluxDB 3's SQL.
    Influxql,
    Cql,
    Mongodb,
    Couchdb,
    /// Elasticsearch, OpenSearch and Solr consoles.
    Search,
    Redis,
    Etcd,
    Cypher,
}

#[derive(Debug, Clone, Serialize)]
pub struct Rule {
    pub id: &'static str,
    pub severity: Severity,
    pub groups: &'static [Group],
}

use Group as G;
use Severity::{Error as E, Info as I, Warning as W};

const fn rule(id: &'static str, severity: Severity, groups: &'static [Group]) -> Rule {
    Rule { id, severity, groups }
}

/// Every rule, in the order Settings lists them.
pub const RULES: &[Rule] = &[
    rule("select-star", I, &[G::Sql, G::Cql]),
    rule("dml-without-where", E, &[G::Sql, G::Cql]),
    rule("not-in-subquery", W, &[G::Sql]),
    rule("leading-wildcard", W, &[G::Sql, G::Cql, G::Search]),
    rule("function-on-column", W, &[G::Sql]),
    rule("equals-null", W, &[G::Sql]),
    rule("order-by-ordinal", I, &[G::Sql]),
    rule("order-by-random", W, &[G::Sql]),
    rule("implicit-cross-join", W, &[G::Sql]),
    rule("insert-without-columns", W, &[G::Sql]),
    rule("distinct-group-by", I, &[G::Sql]),
    rule("union-distinct", I, &[G::Sql]),
    rule("nolock", W, &[G::Tsql]),
    rule("cursor", I, &[G::Tsql]),
    rule("set-rowcount", W, &[G::Tsql]),
    rule("global-identity", W, &[G::Tsql]),
    rule("sp-prefix", W, &[G::Tsql]),
    rule("set-nocount", I, &[G::Tsql]),
    rule("for-update-wait", I, &[G::Postgres, G::Mysql, G::Oracle]),
    rule("serial-identity", I, &[G::Postgres]),
    rule("group-by-nonaggregated", W, &[G::Mysql]),
    rule("rownum-order-by", W, &[G::Oracle]),
    rule("outer-join-plus", I, &[G::Oracle]),
    rule("delete-without-time", W, &[G::Influxql]),
    rule("drop-series", W, &[G::Influxql]),
    rule("allow-filtering", W, &[G::Cql]),
    rule("no-partition-key", W, &[G::Cql]),
    rule("batch-partitions", I, &[G::Cql]),
    rule("write-all", E, &[G::Mongodb, G::Search, G::Etcd]),
    rule("where-operator", W, &[G::Mongodb]),
    rule("unanchored-regex", W, &[G::Mongodb, G::Couchdb]),
    rule("read-all", I, &[G::Mongodb, G::Couchdb, G::Etcd]),
    rule("keys-command", W, &[G::Redis]),
    rule("flush", E, &[G::Redis]),
    rule("big-read", I, &[G::Redis]),
    rule("match-without-label", W, &[G::Cypher]),
    rule("cartesian-product", W, &[G::Cypher]),
    rule("detach-delete-all", E, &[G::Cypher]),
];

pub fn severity(rule: &str) -> Severity {
    RULES.iter().find(|r| r.id == rule).map_or(Severity::Info, |r| r.severity)
}

/// One problem: a rule, where (byte offsets in the script) and the values
/// its message shows (`{{name}}`…).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    pub start: usize,
    pub end: usize,
    pub params: Vec<(&'static str, String)>,
}

impl Finding {
    pub fn new(rule: &'static str, start: usize, end: usize) -> Self {
        Self { rule, start, end: end.max(start), params: Vec::new() }
    }

    pub fn param(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.params.push((key, value.into()));
        self
    }
}

/// SQL engines whose own family has extra rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Generic,
    Tsql,
    Postgres,
    Mysql,
    Oracle,
    Influxql,
}

/// Which tokenizer and rules a connection's editor gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Sql(ScriptDialect, Flavor),
    Cql(ScriptDialect),
    Mongodb,
    Couchdb,
    Search,
    Redis,
    Etcd,
    Cypher,
    /// No rules (Flux).
    None,
}

impl Profile {
    /// From the driver's language, its editor dialect hint and its id.
    pub fn of(info: &DriverInfo, dialect: ScriptDialect) -> Self {
        match info.language {
            Language::Sql => Profile::Sql(
                dialect,
                match info.dialect {
                    "mssql" | "sybase" => Flavor::Tsql,
                    "postgres" => Flavor::Postgres,
                    "mysql" => Flavor::Mysql,
                    "oracle" => Flavor::Oracle,
                    "influxql" | "influxdb3" => Flavor::Influxql,
                    _ => Flavor::Generic,
                },
            ),
            Language::Cql => Profile::Cql(dialect),
            Language::Json => match info.id {
                "mongodb" | "ferretdb" | "documentdb" => Profile::Mongodb,
                "couchdb" => Profile::Couchdb,
                _ => Profile::Search,
            },
            Language::Redis if info.dialect == "etcd" => Profile::Etcd,
            Language::Redis => Profile::Redis,
            Language::Cypher => Profile::Cypher,
            Language::Flux => Profile::None,
        }
    }
}

/// Every problem in `script`, in text order.
pub fn lint(script: &str, profile: Profile) -> Vec<Finding> {
    let mut out = Vec::new();
    match profile {
        Profile::Sql(d, flavor) => sql::lint(script, &d, flavor, &mut out),
        Profile::Cql(d) => cql::lint(script, &d, &mut out),
        Profile::Mongodb => mongo::lint(script, &mut out),
        Profile::Couchdb => console::lint_couchdb(script, &mut out),
        Profile::Search => console::lint_search(script, &mut out),
        Profile::Redis => shell::lint_redis(script, &mut out),
        Profile::Etcd => shell::lint_etcd(script, &mut out),
        Profile::Cypher => cypher::lint(script, &mut out),
        Profile::None => {}
    }
    out.sort_by_key(|f| (f.start, f.end));
    out.dedup_by(|a, b| a.rule == b.rule && a.start == b.start);
    out
}

#[cfg(test)]
mod tests;
