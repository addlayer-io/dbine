//! What an object uses (`Session::references`), from `system.tables`.
//!
//! - A view's or materialized view's `create_table_query` (ClickHouse
//!   stores it with its names qualified) read with the contract's
//!   `used_names`; each name is resolved in one `system.tables` query
//!   across every database (Probable). `dictGet('db.dict', …)`,
//!   `joinGet` and the like name a dictionary or Join table in a string:
//!   resolved the same way.
//! - A materialized view's source tables are Confirmed: `system.tables`
//!   lists the view in their `dependencies_table`. Its `TO` table, and a
//!   refreshable view's `DEPENDS ON` views, come from the same statement
//!   and are Confirmed when they resolve.
//! - `loading_dependencies_*` (recent versions): a dictionary's
//!   ClickHouse source table and the dictionaries a table's defaults read
//!   (`dictGet` in a `DEFAULT`). Confirmed.
//! - A table's engine (`engine_full`): Distributed's table (an item when
//!   the cluster has this server, `external` otherwise), the tables a
//!   Merge table matches, Buffer's destination and Dictionary's
//!   dictionary (Confirmed); MySQL, PostgreSQL, MongoDB, S3, URL, Kafka…
//!   tables read outside the server: `external`.
//! - A dictionary's `SOURCE(…)`: a ClickHouse table on this server is an
//!   item, anything else (MySQL, PostgreSQL, HTTP, a file…) `external`.
//! - Table functions in a view (`remote`, `mysql`, `postgresql`, `s3`,
//!   `url`, `file`…) are `external`; `cluster`, `merge` and `dictionary`
//!   resolve like their engines.
//! - A SQL function, and an object `system.tables` doesn't have: the
//!   contract's default (its definition, read and resolved by name).
//!
//! An object in another database of the same server is an item with that
//! database as its schema, which this driver reads (`definition`,
//! `columns`, this). The server hides passwords in definitions and engine
//! arguments (`[HIDDEN]`); of a source only its host, database, table,
//! path or URL is kept, the URL without its user or query string.

use crate::{kind_of, text, ClickHouseSession, Flavor, DICTIONARY};
use dbine_driver::dependencies::{
    references_scan, used_names, Confidence, DependencyReport, DependencyScan, DependencyTarget,
    Dependent, ExternalRef, Mention, Relation, LINE_CHARS, MAX_MENTIONS,
};
use dbine_driver::sql::{code_tokens, NameToken, ScriptDialect, TokenKind};
use dbine_driver::{kinds, Result};
use serde_json::Value;

/// The target's row; `loading_dependencies_*` only on servers that have
/// them (see [`TARGET_SQL_OLD`]).
pub(crate) const TARGET_SQL: &str = "SELECT engine, engine_full, create_table_query,
       loading_dependencies_database, loading_dependencies_table
  FROM system.tables WHERE database = {db:String} AND name = {t:String}";
pub(crate) const TARGET_SQL_OLD: &str = "SELECT engine, engine_full, create_table_query
  FROM system.tables WHERE database = {db:String} AND name = {t:String}";
/// The tables whose `dependencies_*` list the target: a materialized
/// view's sources.
pub(crate) const SOURCES_SQL: &str = "SELECT database, name, engine FROM system.tables
 WHERE arrayExists((d, t) -> d = {db:String} AND t = {t:String}, dependencies_database, dependencies_table)";
/// Whether a cluster has this server.
pub(crate) const LOCAL_CLUSTER_SQL: &str =
    "SELECT count() FROM system.clusters WHERE cluster = {c:String} AND is_local";
/// The tables a Merge table reads: `{db}` exactly or by regexp, `{re}` the
/// tables' regexp.
pub(crate) const MERGE_SQL: &str = "SELECT database, name, engine FROM system.tables
 WHERE database = {db:String} AND match(name, {re:String}) AND NOT (database = {self_db:String} AND name = {self:String})
 ORDER BY database, name LIMIT 1000";
pub(crate) const MERGE_REGEXP_SQL: &str = "SELECT database, name, engine FROM system.tables
 WHERE match(database, {db:String}) AND match(name, {re:String}) AND NOT (database = {self_db:String} AND name = {self:String})
 ORDER BY database, name LIMIT 1000";
/// SQL functions among the names a body calls (they're global).
pub(crate) const FUNCTIONS_SQL: &str =
    "SELECT name FROM system.functions WHERE origin = 'SQLUserDefined'";
/// Names resolved per `system.tables` query.
const RESOLVE_CHUNK: usize = 100;

/// The target's row in `system.tables`.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct TargetRow {
    pub engine: String,
    pub engine_full: String,
    pub create: String,
    /// `loading_dependencies_*`: (database, name).
    pub loading: Vec<(String, String)>,
}

pub(crate) fn target_row(r: &[Value]) -> TargetRow {
    // Arrays come as their JSON text (see `read_rows`).
    let strings = |v: Option<&Value>| -> Vec<String> {
        match v {
            Some(Value::Array(a)) => a.iter().map(text).collect(),
            Some(Value::String(s)) => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
            _ => Vec::new(),
        }
    };
    let dbs = strings(r.get(3));
    let names = strings(r.get(4));
    TargetRow {
        engine: r.first().map(text).unwrap_or_default(),
        engine_full: r.get(1).map(text).unwrap_or_default(),
        create: r.get(2).map(text).unwrap_or_default(),
        loading: dbs.into_iter().zip(names).collect(),
    }
}

/// An object to look up in `system.tables`: what found it, how sure that
/// is and where the body names it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wanted {
    pub database: String,
    pub name: String,
    pub confidence: Confidence,
    pub detail: Option<String>,
    /// Byte offsets in the target's definition.
    pub at: Vec<usize>,
    /// Called (`f(`): a SQL function when no table has the name.
    pub call: bool,
}

/// What the target's definition and engine name, before resolving.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Found {
    pub wanted: Vec<Wanted>,
    pub external: Vec<ExternalRef>,
    /// Distributed tables and `cluster()` calls: the cluster, the table
    /// (database, name) and where it's named.
    pub clusters: Vec<(String, Wanted)>,
    /// Merge tables and `merge()` calls: the database (a regexp when the
    /// flag says so) and the tables' regexp.
    pub merges: Vec<(String, bool, String, Vec<usize>)>,
}

impl Found {
    fn want(&mut self, w: Wanted) {
        match self
            .wanted
            .iter_mut()
            .find(|x| x.database == w.database && x.name == w.name)
        {
            Some(x) => {
                x.confidence = x.confidence.min(w.confidence);
                x.detail = x.detail.take().or(w.detail);
                x.at.extend(w.at);
                x.call |= w.call;
            }
            None => self.wanted.push(w),
        }
    }

    fn outside(&mut self, x: ExternalRef) {
        match self.external.iter_mut().find(|e| same_source(e, &x)) {
            Some(e) => e.mentions.extend(x.mentions),
            None => self.external.push(x),
        }
    }
}

fn same_source(a: &ExternalRef, b: &ExternalRef) -> bool {
    (
        &a.system,
        &a.server,
        &a.database,
        &a.schema,
        &a.name,
        &a.detail,
    ) == (
        &b.system,
        &b.server,
        &b.database,
        &b.schema,
        &b.name,
        &b.detail,
    )
}

fn wanted(db: &str, name: &str, confidence: Confidence, detail: Option<&str>) -> Wanted {
    Wanted {
        database: db.to_string(),
        name: name.to_string(),
        confidence,
        detail: detail.map(str::to_string),
        at: Vec::new(),
        call: false,
    }
}

impl ClickHouseSession {
    pub(crate) async fn references_of(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = &target.object;
        let db = t.schema().unwrap_or(&self.database).to_string();
        let row = if t.kind == kinds::FUNCTION {
            None
        } else {
            self.target_of(&db, &t.name).await
        };
        let Some(row) = row else {
            return references_scan(self, target, scan).await;
        };
        let mut report = DependencyReport::default();
        let mut found = Found::default();
        let code = scan.written_kinds.contains(&t.kind)
            && matches!(t.kind.as_str(), kinds::VIEW | kinds::MATERIALIZED_VIEW);
        if code && !row.create.is_empty() {
            report.scanned = 1;
            code_refs(&row.create, &scan.dialect, &db, &mut found);
        }
        if row.engine == "MaterializedView" {
            for w in view_clauses(&row.create, &scan.dialect, &db) {
                found.want(w);
            }
        }
        // A materialized view's sources; Timeplus also lists plain views'
        // (and a materialized view's `INTO` stream).
        if matches!(t.kind.as_str(), kinds::VIEW | kinds::MATERIALIZED_VIEW) {
            if let Ok(rows) = self.rows(SOURCES_SQL, &[("db", &db), ("t", &t.name)]).await {
                for r in &rows {
                    found.want(wanted(
                        &text(&r[0]),
                        &text(&r[1]),
                        Confidence::Confirmed,
                        None,
                    ));
                }
            }
        }
        engine_refs(&row.engine_full, &db, &mut found);
        if t.kind == DICTIONARY {
            dictionary_source(&row.create, &scan.dialect, &db, &mut found);
        }
        for (d, n) in &row.loading {
            found.want(wanted(d, n, Confidence::Confirmed, None));
        }
        let body = if code { row.create.as_str() } else { "" };
        self.resolve(&mut found, &db, t, body, &mut report).await;
        report.external = std::mem::take(&mut found.external);
        sort(&mut report.items);
        Ok(report)
    }

    /// The target's row: with `loading_dependencies_*`, or without them on
    /// a server that doesn't have them. `None`: not in `system.tables`.
    async fn target_of(&self, db: &str, name: &str) -> Option<TargetRow> {
        let params = [("db", db), ("t", name)];
        let rows = match self.rows(TARGET_SQL, &params).await {
            Ok(r) => r,
            Err(_) => self.rows(TARGET_SQL_OLD, &params).await.ok()?,
        };
        rows.first().map(|r| target_row(r))
    }

    /// Looks up what `found` wants (and the clusters and Merge tables), and
    /// adds the items to `report`.
    async fn resolve(
        &self,
        found: &mut Found,
        db: &str,
        target: &dbine_driver::ObjectRef,
        body: &str,
        report: &mut DependencyReport,
    ) {
        for (cluster, w) in std::mem::take(&mut found.clusters) {
            let local = self
                .rows(LOCAL_CLUSTER_SQL, &[("c", &cluster)])
                .await
                .ok()
                .and_then(|r| r.first().and_then(|r| crate::jobs::num(r.first())))
                .is_some_and(|n| n > 0.0);
            if local {
                found.want(w);
            } else {
                found.outside(ExternalRef {
                    system: Some("ClickHouse".into()),
                    server: Some(cluster),
                    database: Some(w.database),
                    name: Some(w.name),
                    detail: w.detail,
                    mentions: mentions_at(body, &w.at),
                    ..Default::default()
                });
            }
        }
        for (mdb, regexp, re, at) in std::mem::take(&mut found.merges) {
            let sql = if regexp { MERGE_REGEXP_SQL } else { MERGE_SQL };
            let params = [
                ("db", mdb.as_str()),
                ("re", re.as_str()),
                ("self_db", db),
                ("self", target.name.as_str()),
            ];
            if let Ok(rows) = self.rows(sql, &params).await {
                for r in &rows {
                    let mut w = wanted(
                        &text(&r[0]),
                        &text(&r[1]),
                        Confidence::Confirmed,
                        Some(&format!("Merge('{re}')")),
                    );
                    w.at.clone_from(&at);
                    found.want(w);
                }
            }
        }
        let mut tables: Vec<(String, String, String)> = Vec::new();
        for chunk in found.wanted.chunks(RESOLVE_CHUNK) {
            let (sql, params) = resolve_sql(chunk);
            let params: Vec<(&str, &str)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            if let Ok(rows) = self.rows(&sql, &params).await {
                tables.extend(rows.iter().map(|r| (text(&r[0]), text(&r[1]), text(&r[2]))));
            }
        }
        let calls: Vec<&str> = found
            .wanted
            .iter()
            .filter(|w| w.call)
            .map(|w| w.name.as_str())
            .collect();
        let functions: Vec<String> = if calls.is_empty() {
            Vec::new()
        } else {
            self.rows(FUNCTIONS_SQL, &[])
                .await
                .map(|rows| rows.iter().filter_map(|r| r.first()).map(text).collect())
                .unwrap_or_default()
        };
        for d in resolved(
            &found.wanted,
            &tables,
            &functions,
            &self.database,
            target,
            body,
            self.flavor,
        ) {
            add(&mut report.items, d);
        }
    }
}

/// One `system.tables` query for these (database, name) pairs, with its
/// parameters.
pub(crate) fn resolve_sql(wanted: &[Wanted]) -> (String, Vec<(String, String)>) {
    let mut params = Vec::new();
    let mut terms = Vec::new();
    for (i, w) in wanted.iter().enumerate() {
        terms.push(format!(
            "(database = {{d{i}:String}} AND name = {{n{i}:String}})"
        ));
        params.push((format!("d{i}"), w.database.clone()));
        params.push((format!("n{i}"), w.name.clone()));
    }
    let sql = format!(
        "SELECT database, name, engine FROM system.tables WHERE {}",
        if terms.is_empty() {
            "0".to_string()
        } else {
            terms.join(" OR ")
        }
    );
    (sql, params)
}

/// What `wanted` resolved to: `tables` (database, name, engine) and the
/// server's SQL `functions`. The session's database has no schema;
/// another database is the item's schema. The target itself is left out.
pub(crate) fn resolved(
    wanted: &[Wanted],
    tables: &[(String, String, String)],
    functions: &[String],
    session_db: &str,
    target: &dbine_driver::ObjectRef,
    body: &str,
    flavor: Flavor,
) -> Vec<Dependent> {
    let target_db = target.schema().unwrap_or(session_db);
    let mut out = Vec::new();
    for w in wanted {
        let kind = match tables
            .iter()
            .find(|(d, n, _)| *d == w.database && *n == w.name)
        {
            Some((_, _, engine)) => kind_of(engine, flavor).to_string(),
            None if w.call && functions.contains(&w.name) => kinds::FUNCTION.to_string(),
            None => continue,
        };
        let function = kind == kinds::FUNCTION;
        if !function && w.database == target_db && w.name == target.name {
            continue;
        }
        let schema = (!function && w.database != session_db).then(|| w.database.clone());
        add(
            &mut out,
            Dependent {
                kind,
                schema,
                name: w.name.clone(),
                parent: None,
                relation: Relation::Code,
                confidence: w.confidence,
                detail: w.detail.clone(),
                mentions: mentions_at(body, &w.at),
            },
        );
    }
    out
}

/// The names `body` (a view's CREATE statement) uses: tables, views and
/// dictionaries (qualified or in `db`), functions called, and the table
/// functions that read elsewhere.
pub(crate) fn code_refs(body: &str, dialect: &ScriptDialect, db: &str, found: &mut Found) {
    let toks = code_tokens(body, dialect);
    let from = query_start(body, &toks);
    let mut functions = Vec::new();
    // Table functions (`remote(`, `merge(`… some are keywords to
    // `used_names`) and dictionaries or Join tables named in a call.
    for (i, t) in toks.iter().enumerate() {
        let call = t.kind == TokenKind::Name
            && t.start >= from
            && toks
                .get(i + 1)
                .is_some_and(|p| p.kind == TokenKind::Punct && p.text == "(")
            && !(i > 0 && toks[i - 1].kind == TokenKind::Punct && toks[i - 1].text == ".");
        if !call {
            continue;
        }
        let lower = t.text.to_ascii_lowercase();
        let args = call_args(body, &toks, i + 1);
        if is_table_function(&lower) {
            engine_source(&lower, &args, &[], db, Some(t.start), body, found);
            functions.push(t.start);
        } else if is_dict_call(&lower) {
            let full = match args.into_iter().next() {
                Some(Arg::Str(v) | Arg::Name(v) | Arg::Expr(v)) => v.replace('`', ""),
                _ => continue,
            };
            let (d, n) = split_qualified(&full, db);
            let mut w = wanted(&d, &n, Confidence::Probable, None);
            w.at.push(t.start);
            found.want(w);
            functions.push(t.start);
        }
    }
    for u in used_names(body, dialect) {
        if u.at < from || functions.contains(&u.at) {
            continue;
        }
        let (database, name) = match u.quals.as_slice() {
            [] => (db.to_string(), u.name.clone()),
            [d] => (d.clone(), u.name.clone()),
            // `a.b.c`: a column or a nested name, not an object.
            _ => continue,
        };
        found.want(Wanted {
            database,
            name,
            confidence: Confidence::Probable,
            detail: None,
            at: vec![u.at],
            call: u.call && u.quals.is_empty(),
        });
    }
}

/// Where a CREATE statement's query starts (its first `AS` outside
/// brackets), so the view's own name and its column types aren't read as
/// names it uses; 0 for a bare query.
fn query_start(body: &str, toks: &[NameToken<'_>]) -> usize {
    let word = |t: &NameToken<'_>, w: &str| {
        t.kind == TokenKind::Name
            && t.text.eq_ignore_ascii_case(w)
            && !matches!(body.as_bytes().get(t.start), Some(b'`' | b'"'))
    };
    if !toks.first().is_some_and(|t| word(t, "CREATE")) {
        return 0;
    }
    let mut depth = 0i32;
    for t in toks {
        match (t.kind, t.text) {
            (TokenKind::Punct, "(") => depth += 1,
            (TokenKind::Punct, ")") => depth -= 1,
            _ if depth == 0 && word(t, "AS") => return t.end,
            _ => {}
        }
    }
    0
}

/// A table function this reads (by its lower-case name).
fn is_table_function(name: &str) -> bool {
    matches!(
        name,
        "cluster" | "clusterallreplicas" | "merge" | "dictionary"
    ) || (name != "file engine" && SOURCES.iter().any(|(n, _)| *n == name))
}

fn is_dict_call(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("dictget")
        || matches!(
            n.as_str(),
            "dicthas"
                | "dictisin"
                | "dictgethierarchy"
                | "dictgetchildren"
                | "dictgetdescendants"
                | "joinget"
                | "joingetornull"
        )
}

/// `db.name` or `name` (in `db`).
fn split_qualified(full: &str, db: &str) -> (String, String) {
    match full.split_once('.') {
        Some((d, n)) if !d.is_empty() && !n.is_empty() => (d.to_string(), n.to_string()),
        _ => (db.to_string(), full.to_string()),
    }
}

/// One argument of an engine or a table function.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Arg {
    /// `'text'`, unquoted.
    Str(String),
    /// A name (`` `x` `` unquoted) or a number.
    Name(String),
    /// `key = value` (named collections, settings), the key in lower case.
    Key(String, String),
    /// Anything else, as written (`currentDatabase()`, `REGEXP('…')`).
    Expr(String),
}

impl Arg {
    /// The text it gives: a string's or a name's, a `key = value`'s value.
    fn value(&self) -> &str {
        match self {
            Arg::Str(s) | Arg::Name(s) | Arg::Expr(s) => s,
            Arg::Key(_, v) => v,
        }
    }
}

/// The arguments of the call whose `(` is token `open`, split at its own
/// commas.
pub(crate) fn call_args(src: &str, toks: &[NameToken<'_>], open: usize) -> Vec<Arg> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut from = open + 1;
    for i in open..toks.len() {
        let t = &toks[i];
        if t.kind != TokenKind::Punct {
            continue;
        }
        match t.text {
            "(" | "[" => depth += 1,
            ")" | "]" => {
                depth -= 1;
                if depth == 0 {
                    if i > from {
                        out.push(arg(src, &toks[from..i]));
                    }
                    break;
                }
            }
            "," if depth == 1 => {
                out.push(arg(src, &toks[from..i]));
                from = i + 1;
            }
            _ => {}
        }
    }
    out
}

/// `key = value, key = value` after the `SETTINGS` of an engine clause.
pub(crate) fn settings(src: &str, dialect: &ScriptDialect) -> Vec<(String, String)> {
    let toks = code_tokens(src, dialect);
    let mut depth = 0i32;
    let Some(start) = toks.iter().position(|t| {
        match (t.kind, t.text) {
            (TokenKind::Punct, "(") => depth += 1,
            (TokenKind::Punct, ")") => depth -= 1,
            _ => {}
        }
        depth == 0 && t.kind == TokenKind::Name && t.text.eq_ignore_ascii_case("SETTINGS")
    }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut from = start + 1;
    let mut depth = 0i32;
    for i in start + 1..=toks.len() {
        let end = match toks.get(i) {
            None => true,
            Some(t) if t.kind == TokenKind::Punct => {
                match t.text {
                    "(" => depth += 1,
                    ")" => depth -= 1,
                    _ => {}
                }
                depth == 0 && t.text == ","
            }
            _ => false,
        };
        if end {
            if let Arg::Key(k, v) = arg(src, &toks[from..i]) {
                out.push((k, v));
            }
            from = i + 1;
        }
    }
    out
}

fn arg(src: &str, toks: &[NameToken<'_>]) -> Arg {
    let quoted = |t: &NameToken<'_>| matches!(src.as_bytes().get(t.start), Some(b'`' | b'"'));
    match toks {
        [t] if t.kind == TokenKind::String => Arg::Str(unquote(t.text)),
        [t] if t.kind == TokenKind::Name => Arg::Name(t.text.to_string()),
        [k, eq, rest @ ..]
            if k.kind == TokenKind::Name
                && !quoted(k)
                && eq.kind == TokenKind::Punct
                && eq.text == "="
                && !rest.is_empty() =>
        {
            Arg::Key(
                k.text.to_ascii_lowercase(),
                arg(src, rest).value().to_string(),
            )
        }
        [] => Arg::Expr(String::new()),
        [t] => Arg::Expr(src[t.start..t.end].to_string()),
        [first, .., last] => Arg::Expr(src[first.start..last.end].to_string()),
    }
}

/// A string literal's text: quotes off, `''` and backslash escapes read.
pub(crate) fn unquote(s: &str) -> String {
    let inner = s
        .strip_prefix('\'')
        .and_then(|x| x.strip_suffix('\''))
        .unwrap_or(s);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        n => n,
                    });
                }
            }
            '\'' if chars.peek() == Some(&'\'') => {
                chars.next();
                out.push('\'');
            }
            c => out.push(c),
        }
    }
    out
}

/// Where a source outside the server is, by engine or table function
/// (lower case): the positions of each part in the arguments.
struct Source {
    system: &'static str,
    server: Option<usize>,
    database: Option<usize>,
    schema: Option<usize>,
    name: Option<usize>,
}

const fn src(
    system: &'static str,
    server: Option<usize>,
    database: Option<usize>,
    schema: Option<usize>,
    name: Option<usize>,
) -> Source {
    Source {
        system,
        server,
        database,
        schema,
        name,
    }
}

/// Engines and table functions that read outside the server.
const SOURCES: &[(&str, Source)] = &[
    ("mysql", src("MySQL", Some(0), Some(1), None, Some(2))),
    (
        "postgresql",
        src("PostgreSQL", Some(0), Some(1), Some(5), Some(2)),
    ),
    (
        "materializedpostgresql",
        src("PostgreSQL", Some(0), Some(1), None, Some(2)),
    ),
    ("mongodb", src("MongoDB", Some(0), Some(1), None, Some(2))),
    ("redis", src("Redis", Some(0), None, None, None)),
    ("sqlite", src("SQLite", None, Some(0), None, Some(1))),
    ("jdbc", src("JDBC", Some(0), Some(1), None, Some(2))),
    ("odbc", src("ODBC", Some(0), Some(1), None, Some(2))),
    ("remote", src("ClickHouse", Some(0), Some(1), None, Some(2))),
    (
        "remotesecure",
        src("ClickHouse", Some(0), Some(1), None, Some(2)),
    ),
    ("s3", src("S3", None, None, None, Some(0))),
    ("s3queue", src("S3", None, None, None, Some(0))),
    ("s3cluster", src("S3", None, None, None, Some(1))),
    (
        "gcs",
        src("Google Cloud Storage", None, None, None, Some(0)),
    ),
    ("oss", src("OSS", None, None, None, Some(0))),
    ("cosn", src("COS", None, None, None, Some(0))),
    ("hdfs", src("HDFS", None, None, None, Some(0))),
    ("hdfscluster", src("HDFS", None, None, None, Some(1))),
    ("url", src("URL", None, None, None, Some(0))),
    ("urlcluster", src("URL", None, None, None, Some(1))),
    ("file", src("File", None, None, None, Some(0))),
    ("filecluster", src("File", None, None, None, Some(1))),
    // The File engine: `File(format[, path])` (without a path, the table's
    // own directory: nothing outside).
    ("file engine", src("File", None, None, None, Some(1))),
    // `ExternalDistributed('MySQL', 'host:port|…', 'db', 'table', …)`.
    (
        "externaldistributed",
        src("", Some(1), Some(2), None, Some(3)),
    ),
    (
        "azureblobstorage",
        src("Azure Blob Storage", None, Some(1), None, Some(2)),
    ),
    (
        "azurequeue",
        src("Azure Blob Storage", None, Some(1), None, Some(2)),
    ),
    (
        "azureblobstoragecluster",
        src("Azure Blob Storage", None, Some(2), None, Some(3)),
    ),
    ("iceberg", src("Iceberg", None, None, None, Some(0))),
    ("icebergs3", src("Iceberg", None, None, None, Some(0))),
    ("iceberglocal", src("Iceberg", None, None, None, Some(0))),
    ("iceberghdfs", src("Iceberg", None, None, None, Some(0))),
    ("deltalake", src("Delta Lake", None, None, None, Some(0))),
    ("deltalakes3", src("Delta Lake", None, None, None, Some(0))),
    ("hudi", src("Hudi", None, None, None, Some(0))),
    ("kafka", src("Kafka", Some(0), None, None, Some(1))),
    ("rabbitmq", src("RabbitMQ", Some(0), None, None, Some(1))),
    ("nats", src("NATS", Some(0), None, None, Some(1))),
    // Timeplus: `SETTINGS type = 'kafka', brokers = '…', topic = '…'` and
    // `type = 'clickhouse', address = '…', database = '…', table = '…'`.
    ("externalstream", src("Kafka", None, None, None, None)),
    ("externaltable", src("ClickHouse", None, None, None, None)),
];

/// Named collections and settings: the keys of each part.
const SERVER_KEYS: &[&str] = &[
    "host",
    "hostname",
    "addresses_expr",
    "kafka_broker_list",
    "rabbitmq_host_port",
    "rabbitmq_address",
    "nats_url",
    "uri",
    "brokers",
    "address",
];
const DATABASE_KEYS: &[&str] = &["database", "db", "container"];
const SCHEMA_KEYS: &[&str] = &["schema"];
const NAME_KEYS: &[&str] = &[
    "table",
    "collection",
    "url",
    "path",
    "filename",
    "blob_path",
    "kafka_topic_list",
    "rabbitmq_exchange_name",
    "nats_subjects",
    "topic",
];

/// A source outside the server from an engine's or table function's
/// arguments (and settings).
fn external_source(
    function: &str,
    args: &[Arg],
    settings: &[(String, String)],
) -> Option<ExternalRef> {
    let (_, s) = SOURCES.iter().find(|(n, _)| *n == function)?;
    let positional: Vec<&Arg> = args.iter().filter(|a| !matches!(a, Arg::Key(..))).collect();
    let keyed = |keys: &[&str]| {
        args.iter()
            .filter_map(|a| match a {
                Arg::Key(k, v) => Some((k.as_str(), v.as_str())),
                _ => None,
            })
            .chain(settings.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .find(|(k, _)| keys.contains(k))
            .map(|(_, v)| v.to_string())
    };
    // A named collection comes first: a name, not a string.
    let named = matches!(positional.first(), Some(Arg::Name(_)))
        && args.iter().any(|a| matches!(a, Arg::Key(..)));
    let at = |i: Option<usize>| -> Option<String> {
        if named {
            return None;
        }
        let a = positional.get(i?)?;
        match a {
            Arg::Str(v) | Arg::Name(v) => Some(v.clone()).filter(|v| !v.is_empty()),
            _ => None,
        }
    };
    let mut server = at(s.server).or_else(|| keyed(SERVER_KEYS));
    if let (Some(h), Some(p)) = (&server, keyed(&["port"])) {
        if !h.contains(':') {
            server = Some(format!("{h}:{p}"));
        }
    }
    let mut x = ExternalRef {
        system: Some(s.system.to_string()),
        server: server.map(|v| clean_location(&v)).filter(|v| !v.is_empty()),
        database: at(s.database).or_else(|| keyed(DATABASE_KEYS)),
        schema: at(s.schema).or_else(|| keyed(SCHEMA_KEYS)),
        name: at(s.name)
            .or_else(|| keyed(NAME_KEYS))
            .map(|v| clean_location(&v)),
        detail: named
            .then(|| positional.first().map(|a| a.value().to_string()))
            .flatten(),
        mentions: Vec::new(),
    };
    match function {
        // `MongoDB('mongodb://…/db', 'collection')`.
        "mongodb" if x.server.as_deref().is_some_and(|v| v.contains("://")) && x.name.is_none() => {
            x.name = x.database.take();
        }
        // `remote('host', 'db.table')`.
        "remote" | "remotesecure" if x.name.is_none() => {
            if let Some(full) = x.database.take() {
                let (d, n) = split_qualified(&full, "");
                x.database = Some(d).filter(|d| !d.is_empty());
                x.name = Some(n);
            }
        }
        // A DSN, never a connection string with its password.
        "odbc" => x.server = x.server.as_deref().and_then(odbc_dsn),
        "jdbc" => {
            if x.server.as_deref().is_some_and(|v| v.contains('=')) {
                x.server = None;
            }
        }
        "externaldistributed" => {
            x.system = positional.first().map(|a| a.value().to_string());
        }
        "file engine" if x.name.is_none() => return None,
        "externalstream" | "externaltable" => {
            if let Some(t) = keyed(&["type"]) {
                x.system = Some(
                    match t.to_ascii_lowercase().as_str() {
                        "kafka" => "Kafka",
                        "redpanda" => "Redpanda",
                        "pulsar" => "Pulsar",
                        "clickhouse" => "ClickHouse",
                        "mysql" => "MySQL",
                        "postgresql" => "PostgreSQL",
                        "s3" => "S3",
                        "iceberg" => "Iceberg",
                        "timeplus" => "Timeplus",
                        _ => t.as_str(),
                    }
                    .to_string(),
                );
            }
        }
        _ => {}
    }
    Some(x)
}

/// A URL or path as shown: without its user info, query string or
/// fragment (presigned URLs and tokens live there).
pub(crate) fn clean_location(v: &str) -> String {
    let v = v.split(['?', '#']).next().unwrap_or(v);
    match v.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = match rest.find('/') {
                Some(p) => rest.split_at(p),
                None => (rest, ""),
            };
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            format!("{scheme}://{host}{path}")
        }
        None => v.to_string(),
    }
}

/// The `DSN=` of an ODBC connection string; a bare name is the DSN.
fn odbc_dsn(v: &str) -> Option<String> {
    if !v.contains('=') {
        return Some(v.to_string());
    }
    v.split(';').find_map(|p| {
        let (k, val) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("dsn")
            .then(|| val.trim().to_string())
    })
}

/// What an engine (`engine_full`, `Distributed('c', 'db', 't', …)`) or a
/// table function reads. `true` when it's one of them.
fn engine_source(
    function: &str,
    args: &[Arg],
    settings: &[(String, String)],
    db: &str,
    at: Option<usize>,
    body: &str,
    found: &mut Found,
) -> bool {
    let database = |a: Option<&Arg>| match a {
        Some(Arg::Str(v) | Arg::Name(v)) => v.clone(),
        _ => db.to_string(),
    };
    let at_list: Vec<usize> = at.into_iter().collect();
    match function {
        "distributed" | "cluster" | "clusterallreplicas" => {
            let Some(cluster) = args.first().map(|a| a.value().to_string()) else {
                return true;
            };
            let (d, n) = match (args.get(1), args.get(2)) {
                (Some(a), Some(Arg::Str(t) | Arg::Name(t))) => (database(Some(a)), t.clone()),
                (Some(Arg::Str(full) | Arg::Name(full)), _) => split_qualified(full, db),
                _ => return true,
            };
            let detail = (function == "distributed").then(|| format!("Distributed('{cluster}')"));
            let mut w = wanted(&d, &n, Confidence::Confirmed, detail.as_deref());
            if at.is_some() {
                w.confidence = Confidence::Probable;
                w.detail = Some(format!("{function}('{cluster}')"));
            }
            w.at = at_list;
            found.clusters.push((cluster, w));
            true
        }
        "merge" => {
            let (mdb, regexp) = match args.first() {
                Some(Arg::Expr(e)) if e.to_ascii_uppercase().starts_with("REGEXP") => {
                    let inner = e
                        .split_once('(')
                        .map(|(_, r)| r.trim_end_matches(')').trim())
                        .unwrap_or_default();
                    (unquote(inner), true)
                }
                a => (database(a), false),
            };
            if let Some(re) = args.get(1).map(|a| a.value().to_string()) {
                found.merges.push((mdb, regexp, re, at_list));
            }
            true
        }
        "buffer" => {
            if let Some(Arg::Str(t) | Arg::Name(t)) = args.get(1) {
                let mut w = wanted(
                    &database(args.first()),
                    t,
                    Confidence::Confirmed,
                    Some("Buffer"),
                );
                w.at = at_list;
                found.want(w);
            }
            true
        }
        "dictionary" => {
            if let Some(Arg::Str(full) | Arg::Name(full)) = args.first() {
                let (d, n) = split_qualified(full, db);
                let confidence = if at.is_some() {
                    Confidence::Probable
                } else {
                    Confidence::Confirmed
                };
                let mut w = wanted(&d, &n, confidence, Some("Dictionary"));
                w.at = at_list;
                found.want(w);
            }
            true
        }
        // `Dictionary(db.dict)` comes back as a name chain: `db`, `.`, `dict`.
        _ => match external_source(function, args, settings) {
            Some(mut x) => {
                x.mentions = mentions_at(body, &at_list);
                found.outside(x);
                true
            }
            None => false,
        },
    }
}

/// What a table's engine (`engine_full`) reads.
pub(crate) fn engine_refs(engine_full: &str, db: &str, found: &mut Found) {
    let dialect = crate::dialect();
    let mut toks = code_tokens(engine_full, &dialect);
    // Timeplus writes it whole: `ENGINE = Stream(1, rand()) …`.
    if toks
        .first()
        .is_some_and(|t| t.text.eq_ignore_ascii_case("ENGINE"))
        && toks.get(1).is_some_and(|t| t.text == "=")
    {
        toks.drain(..2);
    }
    let Some(first) = toks.first().filter(|t| t.kind == TokenKind::Name) else {
        return;
    };
    let function = match first.text.to_ascii_lowercase() {
        f if f == "file" => "file engine".to_string(),
        f => f,
    };
    let mut args = match toks.get(1) {
        Some(t) if t.kind == TokenKind::Punct && t.text == "(" => call_args(engine_full, &toks, 1),
        _ => Vec::new(),
    };
    // `Dictionary(db.dict)`: one name with a dot.
    if function == "dictionary" {
        if let Some(Arg::Expr(e)) = args.first() {
            let full = e.replace('`', "");
            args = vec![Arg::Str(full)];
        }
    }
    let settings = settings(engine_full, &dialect);
    engine_source(&function, &args, &settings, db, None, "", found);
}

/// A materialized view's `TO db.table` and a refreshable one's `DEPENDS
/// ON a, b` (before its `AS`), unqualified names in `db`.
pub(crate) fn view_clauses(create: &str, dialect: &ScriptDialect, db: &str) -> Vec<Wanted> {
    let toks = code_tokens(create, dialect);
    let word = |i: usize, w: &str| {
        toks.get(i).is_some_and(|t| {
            t.kind == TokenKind::Name
                && t.text.eq_ignore_ascii_case(w)
                && !matches!(create.as_bytes().get(t.start), Some(b'`' | b'"'))
        })
    };
    // `a.b` from `i`: the parts and the index after them.
    let chain = |i: usize| -> Option<(Vec<&str>, usize)> {
        let mut parts = vec![toks.get(i).filter(|t| t.kind == TokenKind::Name)?.text];
        let mut j = i + 1;
        while toks
            .get(j)
            .is_some_and(|t| t.kind == TokenKind::Punct && t.text == ".")
            && toks.get(j + 1).is_some_and(|t| t.kind == TokenKind::Name)
        {
            parts.push(toks[j + 1].text);
            j += 2;
        }
        Some((parts, j))
    };
    let at_name = |parts: &[&str], start: usize, detail: &str| -> Option<Wanted> {
        let (d, n) = match parts {
            [n] => (db, *n),
            [d, n] => (*d, *n),
            _ => return None,
        };
        let mut w = wanted(d, n, Confidence::Confirmed, Some(detail));
        w.at.push(start);
        Some(w)
    };
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if t.kind == TokenKind::Punct {
            match t.text {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        if depth != 0 {
            i += 1;
            continue;
        }
        if word(i, "AS") {
            break;
        }
        // `INTO`: Timeplus.
        let into = word(i, "INTO");
        if (word(i, "TO") || into) && !word(i + 1, "INNER") {
            if let Some((parts, next)) = chain(i + 1) {
                out.extend(at_name(
                    &parts,
                    toks[i + 1].start,
                    if into { "INTO" } else { "TO" },
                ));
                i = next;
                continue;
            }
        }
        if word(i, "DEPENDS") && word(i + 1, "ON") {
            let mut j = i + 2;
            while let Some((parts, next)) = chain(j) {
                out.extend(at_name(&parts, toks[j].start, "DEPENDS ON"));
                j = next;
                if toks
                    .get(j)
                    .is_some_and(|t| t.kind == TokenKind::Punct && t.text == ",")
                {
                    j += 1;
                } else {
                    break;
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// A dictionary's `SOURCE(KIND(KEY value …))`: a ClickHouse table on this
/// server is an item (and its `QUERY`'s names); the rest is `external`.
pub(crate) fn dictionary_source(
    create: &str,
    dialect: &ScriptDialect,
    db: &str,
    found: &mut Found,
) {
    let toks = code_tokens(create, dialect);
    let Some(src) = toks.windows(4).position(|w| {
        w[0].kind == TokenKind::Name
            && w[0].text.eq_ignore_ascii_case("SOURCE")
            && w[1].text == "("
            && w[2].kind == TokenKind::Name
            && w[3].text == "("
    }) else {
        return;
    };
    let kind = toks[src + 2].text.to_ascii_uppercase();
    // `KEY value` pairs, keys in upper case; nested blocks (`REPLICA(…)`)
    // skipped.
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut i = src + 4;
    let mut depth = 1i32;
    while i < toks.len() && depth > 0 {
        let t = &toks[i];
        if t.kind == TokenKind::Punct {
            match t.text {
                "(" => depth += 1,
                ")" => depth -= 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        if depth == 1 && t.kind == TokenKind::Name {
            if let Some(v) = toks.get(i + 1).filter(|v| v.kind != TokenKind::Punct) {
                let value = match v.kind {
                    TokenKind::String => unquote(v.text),
                    _ => v.text.to_string(),
                };
                pairs.push((t.text.to_ascii_uppercase(), value));
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let host = get("HOST");
    let local_host = host
        .as_deref()
        .is_none_or(|h| matches!(h, "localhost" | "127.0.0.1" | "::1"));
    if kind == "CLICKHOUSE" && local_host && get("NAME").is_none() {
        let sdb = get("DB").unwrap_or_else(|| db.to_string());
        if let Some(table) = get("TABLE") {
            found.want(wanted(&sdb, &table, Confidence::Confirmed, Some("SOURCE")));
        }
        if let Some(query) = get("QUERY") {
            let mut inner = Found::default();
            code_refs(&query, dialect, &sdb, &mut inner);
            for mut w in inner.wanted {
                w.at.clear();
                w.call = false;
                found.want(w);
            }
            for mut x in inner.external {
                x.mentions.clear();
                found.outside(x);
            }
        }
        return;
    }
    let system = match kind.as_str() {
        "CLICKHOUSE" => "ClickHouse",
        "MYSQL" => "MySQL",
        "POSTGRESQL" => "PostgreSQL",
        "MONGODB" => "MongoDB",
        "REDIS" => "Redis",
        "HTTP" => "HTTP",
        "FILE" => "File",
        "EXECUTABLE" | "EXECUTABLE_POOL" => "Executable",
        "ODBC" => "ODBC",
        "CASSANDRA" => "Cassandra",
        "YAMLREGEXPTREE" => "YAML",
        "NULL" => return,
        _ => kind.as_str(),
    };
    let server = match (host, get("PORT")) {
        (Some(h), Some(p)) => Some(format!("{h}:{p}")),
        (Some(h), None) => Some(h),
        (None, _) => get("URL")
            .or_else(|| get("URI"))
            .map(|u| clean_location(&u))
            .filter(|_| matches!(kind.as_str(), "MONGODB" | "REDIS")),
    };
    found.outside(ExternalRef {
        system: Some(system.to_string()),
        server,
        database: get("DB").or_else(|| get("KEYSPACE")),
        schema: get("SCHEMA"),
        name: get("TABLE")
            .or_else(|| get("COLLECTION"))
            .or_else(|| get("COLUMN_FAMILY"))
            .or_else(|| {
                get("URL")
                    .filter(|_| !matches!(kind.as_str(), "MONGODB" | "REDIS"))
                    .map(|u| clean_location(&u))
            })
            .or_else(|| get("PATH"))
            .or_else(|| {
                get("COMMAND").map(|c| c.split_whitespace().next().unwrap_or("").to_string())
            }),
        detail: get("NAME"),
        mentions: Vec::new(),
    });
}

/// One mention per line of `body` at these offsets, as the contract keeps
/// them (at most [`MAX_MENTIONS`], each line cut at [`LINE_CHARS`]).
pub(crate) fn mentions_at(body: &str, at: &[usize]) -> Vec<Mention> {
    let mut at: Vec<usize> = at.iter().copied().filter(|&a| a < body.len()).collect();
    at.sort_unstable();
    let mut out: Vec<Mention> = Vec::new();
    for a in at {
        let line = body[..a].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
        if out.last().is_some_and(|m| m.line == line) {
            continue;
        }
        if out.len() == MAX_MENTIONS {
            break;
        }
        let from = body[..a].rfind('\n').map_or(0, |p| p + 1);
        let to = body[a..].find('\n').map_or(body.len(), |p| a + p);
        out.push(Mention {
            line,
            text: body[from..to].trim().chars().take(LINE_CHARS).collect(),
            dynamic: false,
        });
    }
    out
}

/// Adds `d`, or merges it into the same object already there: the surer
/// confidence, the detail and the mentions of both.
pub(crate) fn add(items: &mut Vec<Dependent>, d: Dependent) {
    match items
        .iter_mut()
        .find(|x| x.relation == d.relation && x.schema == d.schema && x.name == d.name)
    {
        Some(x) => {
            x.confidence = x.confidence.min(d.confidence);
            if x.detail.is_none() {
                x.detail = d.detail;
            }
            for m in d.mentions {
                if !x.mentions.iter().any(|y| y.line == m.line) {
                    x.mentions.push(m);
                }
            }
            x.mentions.sort_by_key(|m| m.line);
        }
        None => items.push(d),
    }
}

/// Confirmed first, then by relation and name (as the contract sorts).
pub(crate) fn sort(items: &mut [Dependent]) {
    items.sort_by(|a, b| {
        (
            a.confidence,
            a.relation,
            a.schema.as_deref().unwrap_or(""),
            a.name.to_lowercase(),
        )
            .cmp(&(
                b.confidence,
                b.relation,
                b.schema.as_deref().unwrap_or(""),
                b.name.to_lowercase(),
            ))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;
    use serde_json::json;

    fn found_in(create: &str, db: &str) -> Found {
        let mut f = Found::default();
        code_refs(create, &crate::dialect(), db, &mut f);
        f
    }

    fn engine(engine_full: &str) -> Found {
        let mut f = Found::default();
        engine_refs(engine_full, "shop", &mut f);
        f
    }

    /// What `f` wants that isn't a call (calls are looked up as SQL
    /// functions, and built-ins drop out there).
    fn pairs(f: &Found) -> Vec<(String, String, Confidence)> {
        f.wanted
            .iter()
            .filter(|w| !w.call)
            .map(|w| (w.database.clone(), w.name.clone(), w.confidence))
            .collect()
    }

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: schema.map(str::to_string),
            name: name.into(),
        }
    }

    #[test]
    fn the_target_row_with_and_without_loading_dependencies() {
        let r = target_row(&[
            json!("MergeTree"),
            json!("MergeTree ORDER BY id"),
            json!("CREATE TABLE shop.t (id UInt64) ENGINE = MergeTree ORDER BY id"),
            json!(["shop", "ref"]),
            json!(["dict", "rates"]),
        ]);
        assert_eq!(r.engine, "MergeTree");
        assert_eq!(
            r.loading,
            [
                ("shop".into(), "dict".into()),
                ("ref".into(), "rates".into())
            ]
        );
        // As the driver reads rows: arrays as their JSON text.
        let r = target_row(&[
            json!("MergeTree"),
            json!(""),
            json!(""),
            json!(r#"["shop"]"#),
            json!(r#"["dict"]"#),
        ]);
        assert_eq!(r.loading, [("shop".into(), "dict".into())]);
        let old = target_row(&[
            json!("View"),
            json!(""),
            json!("CREATE VIEW shop.v AS SELECT 1"),
        ]);
        assert!(old.loading.is_empty());
        assert_eq!(old.create, "CREATE VIEW shop.v AS SELECT 1");
    }

    #[test]
    fn the_sql_sent() {
        assert!(TARGET_SQL.contains("loading_dependencies_database, loading_dependencies_table"));
        assert!(!TARGET_SQL_OLD.contains("loading_dependencies"));
        assert!(SOURCES_SQL.contains(
            "arrayExists((d, t) -> d = {db:String} AND t = {t:String}, dependencies_database, dependencies_table)"
        ));
        let (sql, params) = resolve_sql(&[
            wanted("shop", "orders", Confidence::Probable, None),
            wanted("ref", "rates", Confidence::Confirmed, None),
        ]);
        assert_eq!(
            sql,
            "SELECT database, name, engine FROM system.tables WHERE \
             (database = {d0:String} AND name = {n0:String}) OR (database = {d1:String} AND name = {n1:String})"
        );
        assert_eq!(
            params,
            [
                ("d0".to_string(), "shop".to_string()),
                ("n0".into(), "orders".into()),
                ("d1".into(), "ref".into()),
                ("n1".into(), "rates".into())
            ]
        );
        // Nothing wanted: a query that returns nothing.
        assert!(resolve_sql(&[]).0.ends_with("WHERE 0"));
    }

    #[test]
    fn a_view_names_tables_in_its_database_and_others() {
        let create = "CREATE VIEW shop.v (`id` UInt64, `w` String) AS SELECT s.id, o.w FROM shop.src AS s \
                      INNER JOIN ref.other AS o ON o.id = s.id WHERE s.id IN (SELECT id FROM `shop`.`gone`)";
        let f = found_in(create, "shop");
        assert_eq!(
            pairs(&f),
            [
                ("shop".into(), "src".into(), Confidence::Probable),
                ("ref".into(), "other".into(), Confidence::Probable),
                ("shop".into(), "gone".into(), Confidence::Probable)
            ]
        );
        assert!(f.external.is_empty());
        // `gone` doesn't resolve; `src` is in the session's database (no
        // schema), `other` in another one (its schema).
        let tables = [
            (
                "shop".to_string(),
                "src".to_string(),
                "MergeTree".to_string(),
            ),
            ("ref".into(), "other".into(), "View".into()),
        ];
        let items = resolved(
            &f.wanted,
            &tables,
            &[],
            "shop",
            &obj(kinds::VIEW, None, "v"),
            create,
            Flavor::ClickHouse,
        );
        assert_eq!(items.len(), 2);
        assert_eq!(
            (
                items[0].kind.as_str(),
                items[0].schema.as_deref(),
                items[0].name.as_str()
            ),
            (kinds::TABLE, None, "src")
        );
        assert_eq!(
            (
                items[1].kind.as_str(),
                items[1].schema.as_deref(),
                items[1].name.as_str()
            ),
            (kinds::VIEW, Some("ref"), "other")
        );
        assert_eq!(items[0].confidence, Confidence::Probable);
        assert_eq!(items[0].relation, Relation::Code);
        assert_eq!(items[0].mentions.len(), 1);
        assert_eq!(items[0].mentions[0].line, 1);
        // Seen from the other database (the target's schema), the session's
        // own tables still have no schema.
        let items = resolved(
            &f.wanted,
            &tables,
            &[],
            "ref",
            &obj(kinds::VIEW, Some("shop"), "v"),
            create,
            Flavor::ClickHouse,
        );
        assert_eq!(items[0].schema.as_deref(), Some("shop"));
        assert_eq!(items[1].schema, None);
    }

    #[test]
    fn the_target_and_unknown_calls_are_left_out_and_sql_functions_kept() {
        let create = "CREATE VIEW shop.v AS SELECT add_tax(total), lower(name) FROM shop.v2";
        let f = found_in(create, "shop");
        let tables = [("shop".to_string(), "v2".to_string(), "View".to_string())];
        let items = resolved(
            &f.wanted,
            &tables,
            &["add_tax".to_string()],
            "shop",
            &obj(kinds::VIEW, None, "v2"),
            create,
            Flavor::ClickHouse,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(
            (
                items[0].kind.as_str(),
                items[0].schema.as_deref(),
                items[0].name.as_str()
            ),
            (kinds::FUNCTION, None, "add_tax")
        );
    }

    #[test]
    fn table_functions_in_a_view_are_sources_outside() {
        let create = "CREATE VIEW shop.v AS SELECT * FROM remote('ch2:9000', sales, orders, 'u', '[HIDDEN]') \
                      UNION ALL SELECT * FROM mysql('my:3306', 'erp', 'clients', 'u', '[HIDDEN]') \
                      UNION ALL SELECT * FROM s3('https://key:secret@bucket.s3.amazonaws.com/x/*.csv?X-Amz-Signature=abc', NOSIGN, 'CSV', 'id UInt64') \
                      UNION ALL SELECT * FROM postgresql('pg:5432', 'crm', 'leads', 'u', '[HIDDEN]', 'sales') \
                      UNION ALL SELECT * FROM remote('ch3', 'db1.t1') \
                      UNION ALL SELECT * FROM numbers(10)";
        let f = found_in(create, "shop");
        assert!(pairs(&f).is_empty(), "{:?}", f.wanted);
        let x = &f.external;
        assert_eq!(x.len(), 5);
        assert_eq!(
            (
                x[0].system.as_deref(),
                x[0].server.as_deref(),
                x[0].database.as_deref(),
                x[0].name.as_deref()
            ),
            (
                Some("ClickHouse"),
                Some("ch2:9000"),
                Some("sales"),
                Some("orders")
            )
        );
        assert_eq!(
            (
                x[1].system.as_deref(),
                x[1].server.as_deref(),
                x[1].database.as_deref(),
                x[1].name.as_deref()
            ),
            (Some("MySQL"), Some("my:3306"), Some("erp"), Some("clients"))
        );
        assert_eq!(x[2].system.as_deref(), Some("S3"));
        assert_eq!(
            x[2].name.as_deref(),
            Some("https://bucket.s3.amazonaws.com/x/*.csv")
        );
        assert_eq!(x[2].server, None);
        assert_eq!(
            (
                x[3].system.as_deref(),
                x[3].schema.as_deref(),
                x[3].name.as_deref()
            ),
            (Some("PostgreSQL"), Some("sales"), Some("leads"))
        );
        assert_eq!(
            (
                x[4].server.as_deref(),
                x[4].database.as_deref(),
                x[4].name.as_deref()
            ),
            (Some("ch3"), Some("db1"), Some("t1"))
        );
        for e in x {
            assert_eq!(e.mentions.len(), 1);
            // The mention is the definition's line, as the server shows it.
            let shown = format!(
                "{:?}",
                ExternalRef {
                    mentions: Vec::new(),
                    ..e.clone()
                }
            );
            assert!(
                !shown.contains("HIDDEN")
                    && !shown.contains("secret")
                    && !shown.contains("Signature")
            );
        }
    }

    #[test]
    fn cluster_merge_and_dictionary_calls_resolve_like_their_engines() {
        let create = "CREATE VIEW shop.v AS SELECT * FROM cluster('main', shop, events) \
                      UNION ALL SELECT * FROM merge(currentDatabase(), '^log_') \
                      UNION ALL SELECT * FROM dictionary('ref.rates') \
                      WHERE dictGet('shop.names', 'v', id) != '' AND joinGet(shop.j, 'v', id) = 1";
        let f = found_in(create, "shop");
        assert_eq!(f.clusters.len(), 1);
        assert_eq!(f.clusters[0].0, "main");
        assert_eq!(
            (
                f.clusters[0].1.database.as_str(),
                f.clusters[0].1.name.as_str()
            ),
            ("shop", "events")
        );
        assert_eq!(f.clusters[0].1.confidence, Confidence::Probable);
        assert_eq!(
            f.merges,
            [(
                "shop".to_string(),
                false,
                "^log_".to_string(),
                vec![create.find("merge(").unwrap()]
            )]
        );
        assert_eq!(
            pairs(&f),
            [
                ("ref".into(), "rates".into(), Confidence::Probable),
                ("shop".into(), "names".into(), Confidence::Probable),
                ("shop".into(), "j".into(), Confidence::Probable)
            ]
        );
    }

    #[test]
    fn a_materialized_views_to_table_and_depends_on() {
        let d = crate::dialect();
        let mv = "CREATE MATERIALIZED VIEW shop.mv TO shop.dst (`id` UInt64, `v` String) AS SELECT id, v FROM shop.src";
        let w = view_clauses(mv, &d, "shop");
        assert_eq!(w.len(), 1);
        assert_eq!(
            (w[0].database.as_str(), w[0].name.as_str()),
            ("shop", "dst")
        );
        assert_eq!(w[0].confidence, Confidence::Confirmed);
        assert_eq!(w[0].detail.as_deref(), Some("TO"));
        let rmv = "CREATE MATERIALIZED VIEW shop.daily REFRESH EVERY 1 DAY DEPENDS ON shop.hourly, `ref`.`rates` \
                   TO totals (`d` Date) AS SELECT d FROM shop.hourly";
        let w = view_clauses(rmv, &d, "shop");
        let got: Vec<(&str, &str, Option<&str>)> = w
            .iter()
            .map(|x| (x.database.as_str(), x.name.as_str(), x.detail.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("shop", "hourly", Some("DEPENDS ON")),
                ("ref", "rates", Some("DEPENDS ON")),
                ("shop", "totals", Some("TO"))
            ]
        );
        // Its own table: nothing.
        let own = "CREATE MATERIALIZED VIEW shop.mv2 (`id` UInt64) ENGINE = MergeTree ORDER BY id AS SELECT id FROM shop.src";
        assert!(view_clauses(own, &d, "shop").is_empty());
        let inner = "CREATE MATERIALIZED VIEW shop.mv3 TO INNER UUID 'abc' (`id` UInt64) AS SELECT id FROM shop.src";
        assert!(view_clauses(inner, &d, "shop").is_empty());
        // Timeplus writes `INTO`.
        let tp = "CREATE MATERIALIZED VIEW default.mv INTO default.t\n(\n  `id` uint64\n) AS\nSELECT\n  id\nFROM\n  default.s";
        let w = view_clauses(tp, &d, "default");
        assert_eq!(w.len(), 1);
        assert_eq!(
            (w[0].name.as_str(), w[0].detail.as_deref()),
            ("t", Some("INTO"))
        );
        // Its query's names, with their lines.
        let f = found_in(tp, "default");
        assert_eq!(
            pairs(&f),
            [("default".into(), "s".into(), Confidence::Probable)]
        );
        let items = resolved(
            &f.wanted,
            &[("default".into(), "s".into(), "Stream".into())],
            &[],
            "default",
            &obj(kinds::MATERIALIZED_VIEW, None, "mv"),
            tp,
            Flavor::Timeplus,
        );
        assert_eq!(items[0].kind, kinds::STREAM);
        assert_eq!(
            (
                items[0].mentions[0].line,
                items[0].mentions[0].text.as_str()
            ),
            (8, "default.s")
        );
    }

    #[test]
    fn engines_that_read_other_tables_of_the_server() {
        let f = engine("Distributed('default', 'shop', 'src', rand())");
        assert_eq!(f.clusters.len(), 1);
        assert_eq!(f.clusters[0].0, "default");
        let w = &f.clusters[0].1;
        assert_eq!(
            (w.database.as_str(), w.name.as_str(), w.confidence),
            ("shop", "src", Confidence::Confirmed)
        );
        assert_eq!(w.detail.as_deref(), Some("Distributed('default')"));
        let f = engine("Distributed(main, currentDatabase(), events_local)");
        assert_eq!(
            (
                f.clusters[0].1.database.as_str(),
                f.clusters[0].1.name.as_str()
            ),
            ("shop", "events_local")
        );

        assert_eq!(
            engine("Merge('shop', '^sr')").merges,
            [("shop".to_string(), false, "^sr".to_string(), vec![])]
        );
        assert_eq!(
            engine("Merge(REGEXP('^logs_'), '^hits')").merges,
            [("^logs_".to_string(), true, "^hits".to_string(), vec![])]
        );
        assert_eq!(
            pairs(&engine(
                "Buffer('shop', 'src', 1, 10, 100, 10000, 1000000, 10000000, 100000000)"
            )),
            [("shop".into(), "src".into(), Confidence::Confirmed)]
        );
        assert_eq!(
            pairs(&engine("Dictionary(ref.rates)")),
            [("ref".into(), "rates".into(), Confidence::Confirmed)]
        );
        assert_eq!(
            pairs(&engine("Dictionary(`rates`)")),
            [("shop".into(), "rates".into(), Confidence::Confirmed)]
        );
        // A MergeTree reads nothing else.
        assert_eq!(
            engine("MergeTree ORDER BY id SETTINGS index_granularity = 8192"),
            Found::default()
        );
    }

    #[test]
    fn engines_that_read_outside_the_server() {
        let one = |e: &str| {
            let f = engine(e);
            assert!(f.wanted.is_empty());
            assert_eq!(f.external.len(), 1, "{e}");
            f.external[0].clone()
        };
        let x = one("MySQL('my:3306', 'erp', 'clients', 'u', '[HIDDEN]')");
        assert_eq!(
            (
                x.system.as_deref(),
                x.server.as_deref(),
                x.database.as_deref(),
                x.name.as_deref()
            ),
            (Some("MySQL"), Some("my:3306"), Some("erp"), Some("clients"))
        );
        let x = one("PostgreSQL('pg:5432', 'crm', 'leads', 'u', '[HIDDEN]', 'sales')");
        assert_eq!(
            (
                x.database.as_deref(),
                x.schema.as_deref(),
                x.name.as_deref()
            ),
            (Some("crm"), Some("sales"), Some("leads"))
        );
        let x = one("URL('https://user:[HIDDEN]@files.example/a.csv', 'CSV')");
        assert_eq!(
            (x.system.as_deref(), x.name.as_deref()),
            (Some("URL"), Some("https://files.example/a.csv"))
        );
        let x = one("Kafka SETTINGS kafka_broker_list = 'k1:9092,k2:9092', kafka_topic_list = 'orders', kafka_group_name = 'g', kafka_format = 'JSONEachRow'");
        assert_eq!(
            (x.system.as_deref(), x.server.as_deref(), x.name.as_deref()),
            (Some("Kafka"), Some("k1:9092,k2:9092"), Some("orders"))
        );
        let x = one("Kafka('k1:9092', 'orders', 'g', 'JSONEachRow')");
        assert_eq!(
            (x.server.as_deref(), x.name.as_deref()),
            (Some("k1:9092"), Some("orders"))
        );
        // A named collection: its name as the detail, the rest from keys.
        let x = one("MySQL(erp_creds, table = 'clients', database = 'erp')");
        assert_eq!(
            (
                x.server.as_deref(),
                x.database.as_deref(),
                x.name.as_deref(),
                x.detail.as_deref()
            ),
            (None, Some("erp"), Some("clients"), Some("erp_creds"))
        );
        let x = one("MongoDB('mongodb://u:[HIDDEN]@mongo:27017/app', 'events')");
        assert_eq!(
            (x.server.as_deref(), x.name.as_deref()),
            (Some("mongodb://mongo:27017/app"), Some("events"))
        );
        let x = one("ODBC('DSN=pg;UID=u;PWD=[HIDDEN]', 'crm', 'leads')");
        assert_eq!(x.server.as_deref(), Some("pg"));
        let x = one("File(CSV, '/data/in.csv')");
        assert_eq!(
            (x.system.as_deref(), x.name.as_deref()),
            (Some("File"), Some("/data/in.csv"))
        );
        let x = one("RabbitMQ SETTINGS rabbitmq_address = 'amqp://u:p@rabbit:5672/vh', rabbitmq_exchange_name = 'orders', rabbitmq_format = 'JSONEachRow'");
        assert_eq!(
            (x.server.as_deref(), x.name.as_deref()),
            (Some("amqp://rabbit:5672/vh"), Some("orders"))
        );
        let x = one("ExternalDistributed('PostgreSQL', 'pg1:5432|pg2:5432', 'crm', 'leads', 'u', '[HIDDEN]')");
        assert_eq!(
            (x.system.as_deref(), x.server.as_deref(), x.name.as_deref()),
            (Some("PostgreSQL"), Some("pg1:5432|pg2:5432"), Some("leads"))
        );
        // The table's own directory: nothing outside.
        assert_eq!(engine("File(CSV)"), Found::default());
        // Timeplus: the whole clause, and its external streams and tables.
        assert_eq!(
            engine("\nENGINE = Stream(1, rand())\nPARTITION BY to_YYYYMM(_tp_time)\nSETTINGS index_granularity = 8192"),
            Found::default()
        );
        let x = one("ENGINE = ExternalStream SETTINGS type = 'kafka', brokers = 'k1:9092', topic = 'orders', security_protocol = 'SASL_SSL', password = '[HIDDEN]'");
        assert_eq!(
            (x.system.as_deref(), x.server.as_deref(), x.name.as_deref()),
            (Some("Kafka"), Some("k1:9092"), Some("orders"))
        );
        let x = one("ENGINE = ExternalTable SETTINGS type = 'clickhouse', address = 'ch:9000', database = 'sales', table = 'events', user = 'u', password = '[HIDDEN]'");
        assert_eq!(
            (
                x.system.as_deref(),
                x.server.as_deref(),
                x.database.as_deref(),
                x.name.as_deref()
            ),
            (
                Some("ClickHouse"),
                Some("ch:9000"),
                Some("sales"),
                Some("events")
            )
        );
    }

    #[test]
    fn a_dictionarys_source() {
        let d = crate::dialect();
        let src = |create: &str| {
            let mut f = Found::default();
            dictionary_source(create, &d, "shop", &mut f);
            f
        };
        let f = src("CREATE DICTIONARY shop.dict (`id` UInt64, `v` String) PRIMARY KEY id SOURCE(CLICKHOUSE(TABLE 'src' DB 'ref')) LIFETIME(MIN 0 MAX 0) LAYOUT(FLAT())");
        assert_eq!(
            pairs(&f),
            [("ref".into(), "src".into(), Confidence::Confirmed)]
        );
        assert!(f.external.is_empty());
        let f = src("CREATE DICTIONARY shop.d2 (`id` UInt64) PRIMARY KEY id SOURCE(CLICKHOUSE(HOST 'localhost' PORT 9000 USER 'u' PASSWORD '[HIDDEN]' QUERY 'SELECT id FROM shop.src JOIN ref.other USING (id)')) LIFETIME(0) LAYOUT(FLAT())");
        assert_eq!(
            pairs(&f),
            [
                ("shop".into(), "src".into(), Confidence::Probable),
                ("ref".into(), "other".into(), Confidence::Probable)
            ]
        );
        let f = src("CREATE DICTIONARY shop.d3 (`id` UInt64) PRIMARY KEY id SOURCE(MYSQL(HOST 'my' PORT 3306 USER 'u' PASSWORD '[HIDDEN]' DB 'erp' TABLE 'clients')) LIFETIME(MIN 0 MAX 300) LAYOUT(HASHED())");
        let x = &f.external[0];
        assert_eq!(
            (
                x.system.as_deref(),
                x.server.as_deref(),
                x.database.as_deref(),
                x.name.as_deref()
            ),
            (Some("MySQL"), Some("my:3306"), Some("erp"), Some("clients"))
        );
        assert!(!format!("{x:?}").contains("HIDDEN"));
        let f = src("CREATE DICTIONARY shop.d4 (`id` UInt64) PRIMARY KEY id SOURCE(HTTP(URL 'https://u:p@example.com/d.csv?token=x' FORMAT 'CSV')) LIFETIME(300) LAYOUT(HASHED())");
        assert_eq!(
            (
                f.external[0].system.as_deref(),
                f.external[0].name.as_deref()
            ),
            (Some("HTTP"), Some("https://example.com/d.csv"))
        );
        let f = src("CREATE DICTIONARY shop.d5 (`id` UInt64) PRIMARY KEY id SOURCE(CLICKHOUSE(HOST 'ch2' PORT 9000 DB 'ref' TABLE 'rates')) LIFETIME(300) LAYOUT(HASHED())");
        assert!(f.wanted.is_empty());
        assert_eq!(
            (
                f.external[0].system.as_deref(),
                f.external[0].server.as_deref(),
                f.external[0].name.as_deref()
            ),
            (Some("ClickHouse"), Some("ch2:9000"), Some("rates"))
        );
    }

    #[test]
    fn items_merge_into_the_surest() {
        let item = |confidence, detail: Option<&str>, line| Dependent {
            kind: kinds::TABLE.into(),
            schema: None,
            name: "src".into(),
            parent: None,
            relation: Relation::Code,
            confidence,
            detail: detail.map(str::to_string),
            mentions: vec![Mention {
                line,
                text: String::new(),
                dynamic: false,
            }],
        };
        let mut items = vec![item(Confidence::Probable, None, 2)];
        add(&mut items, item(Confidence::Confirmed, Some("TO"), 1));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].confidence, Confidence::Confirmed);
        assert_eq!(items[0].detail.as_deref(), Some("TO"));
        assert_eq!(
            items[0].mentions.iter().map(|m| m.line).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn mentions_are_the_lines() {
        let body = "SELECT *\nFROM a\nJOIN b ON 1";
        let m = mentions_at(body, &[body.find('b').unwrap(), body.find('a').unwrap()]);
        assert_eq!(
            m.iter()
                .map(|m| (m.line, m.text.as_str()))
                .collect::<Vec<_>>(),
            [(2, "FROM a"), (3, "JOIN b ON 1")]
        );
        assert!(mentions_at("", &[3]).is_empty());
    }

    #[test]
    fn locations_lose_their_secrets() {
        assert_eq!(
            clean_location("https://u:p@h.example/x?sig=1#f"),
            "https://h.example/x"
        );
        assert_eq!(clean_location("s3://bucket/key"), "s3://bucket/key");
        assert_eq!(clean_location("/data/a.csv"), "/data/a.csv");
        assert_eq!(odbc_dsn("DSN=pg;PWD=x").as_deref(), Some("pg"));
        assert_eq!(odbc_dsn("Driver=x;PWD=y"), None);
        assert_eq!(odbc_dsn("pg").as_deref(), Some("pg"));
        assert_eq!(unquote(r"'it''s \'x\''"), "it's 'x'");
    }
}
