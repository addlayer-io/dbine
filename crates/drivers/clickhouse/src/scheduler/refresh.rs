//! ClickHouse's refreshable materialized views as the scheduler's jobs: the
//! whole `CREATE MATERIALIZED VIEW … REFRESH …` statement as a form.
//!
//! - Read: `SHOW CREATE TABLE` (the server's own formatting) parsed into a
//!   [`ViewDef`]; its state (started or stopped) from
//!   `system.view_refreshes`.
//! - Changed in place: `ALTER TABLE … MODIFY QUERY`, `MODIFY REFRESH` (the
//!   whole refresh clause: schedule, `OFFSET`, `RANDOMIZE FOR`, `DEPENDS
//!   ON`, the refresh `SETTINGS`; `APPEND` repeated as it is, since it
//!   can't change; only when the clause means something else, since it
//!   reschedules the view), `MODIFY SQL SECURITY`, `MODIFY COMMENT`,
//!   `RENAME TABLE`, and `SYSTEM START | STOP VIEW` (which the server
//!   doesn't keep across a restart).
//! - Created again: ClickHouse can't change a view's target (`TO`), its
//!   own table's engine, its columns or `APPEND` in place, so a change to
//!   one of them is a `CREATE OR REPLACE MATERIALIZED VIEW` with the whole
//!   new definition (a `DROP VIEW` and a `CREATE` in a database without
//!   `REPLACE`, which creates the old view again when the server refuses
//!   the new one); the script and the form say what that loses.
//! - Deleted: `DROP VIEW`.
//!
//! Intervals are ClickHouse's own (`1 HOUR 30 MINUTE`), checked before
//! anything runs; `DEPENDS ON` names are always written with their
//! database (an unqualified one would be read in the session's database).

use super::lex::{self, Parser, T};
use super::{
    bad, database_field, id_pairs, ids, name_field, one_statement, qi, qname, query_field,
    same_pairs, shown, Ctx, Plan, Restore, Vals,
};
use crate::jobs::{self, job_id, parse_id};
use crate::schema::string_literal;
use crate::ClickHouseSession;
use dbine_driver::{Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const RECREATE_NOTE: &str = "Cambiar el destino, el motor, las columnas o APPEND vuelve a crear la vista (CREATE OR REPLACE): se pierde su estado de refresco y su tabla propia vuelve a empezar vacía, hasta el primer refresco.";
const NO_REPLACE_NOTE: &str = "Esta base de datos no admite CREATE OR REPLACE: cambiar el destino, el motor, las columnas o APPEND borra la vista y la vuelve a crear.";
const MOVE_NOTE: &str = "ClickHouse no mueve a otra base de datos una vista con tabla propia: cambiarle la base de datos la crea en la nueva y borra esta; su tabla propia empieza vacía, hasta el primer refresco.";
const DEPENDS_NOTE: &str =
    "Las vistas que la nombran en DEPENDS ON siguen esperando el nombre anterior.";
const STOP_NOT_KEPT: &str =
    "SYSTEM STOP VIEW la deshabilita hasta que el servidor se reinicia: al arrancar vuelve a refrescar.";

// The refresh settings with a field of their own.
const RETRIES: &str = "refresh_retries";
const BACKOFF: &str = "refresh_retry_initial_backoff_ms";
const MAX_BACKOFF: &str = "refresh_retry_max_backoff_ms";
const ALL_REPLICAS: &str = "all_replicas";
const KNOWN_SETTINGS: [&str; 4] = [RETRIES, BACKOFF, MAX_BACKOFF, ALL_REPLICAS];

// ---------------------------------------------------------------------------
// Intervals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    /// `HOUR`, `hours`, `Hour`…
    pub(crate) fn parse(w: &str) -> Option<Unit> {
        let up = w.to_ascii_uppercase();
        let base = up.strip_suffix('S').unwrap_or(&up);
        Some(match base {
            "SECOND" => Unit::Second,
            "MINUTE" => Unit::Minute,
            "HOUR" => Unit::Hour,
            "DAY" => Unit::Day,
            "WEEK" => Unit::Week,
            "MONTH" => Unit::Month,
            "YEAR" => Unit::Year,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Unit::Second => "SECOND",
            Unit::Minute => "MINUTE",
            Unit::Hour => "HOUR",
            Unit::Day => "DAY",
            Unit::Week => "WEEK",
            Unit::Month => "MONTH",
            Unit::Year => "YEAR",
        }
    }

    /// Months and years: the calendar's units, not the clock's.
    fn calendar(self) -> bool {
        matches!(self, Unit::Month | Unit::Year)
    }

    /// Seconds in one (a clock unit) or months in one (a calendar unit).
    fn size(self) -> u64 {
        match self {
            Unit::Second => 1,
            Unit::Minute => 60,
            Unit::Hour => 3600,
            Unit::Day => 86_400,
            Unit::Week => 604_800,
            Unit::Month => 1,
            Unit::Year => 12,
        }
    }
}

/// `1 HOUR 30 MINUTE` as its parts.
pub(crate) type Interval = Vec<(u64, Unit)>;

/// An interval as ClickHouse writes it (`1 HOUR 30 MINUTE`); `None` when
/// the text isn't one.
pub(crate) fn parse_interval(text: &str) -> Option<Interval> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() || !words.len().is_multiple_of(2) {
        return None;
    }
    words
        .chunks(2)
        .map(|p| {
            let n = p[0];
            if n.is_empty() || n.len() > 9 || !n.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((n.parse().ok()?, Unit::parse(p[1])?))
        })
        .collect()
}

pub(crate) fn interval_text(iv: &[(u64, Unit)]) -> String {
    iv.iter()
        .map(|(n, u)| format!("{n} {}", u.name()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether it's in months (`true`) or seconds, and how many; `None` when
/// it mixes both.
fn total(iv: &[(u64, Unit)]) -> Option<(bool, u64)> {
    let calendar = iv.first()?.1.calendar();
    if iv.iter().any(|(_, u)| u.calendar() != calendar) {
        return None;
    }
    Some((calendar, iv.iter().map(|(n, u)| n * u.size()).sum()))
}

/// An interval field: `None` when empty.
fn interval_field(v: Vals, k: &str, label: &str) -> Result<Option<Interval>> {
    let Some(t) = v.opt(k) else { return Ok(None) };
    let iv = parse_interval(&t).ok_or_else(|| {
        bad(format!(
            "«{label}»: «{}» no es un intervalo de ClickHouse: un número y una unidad (SECOND, MINUTE, HOUR, DAY, WEEK, MONTH o YEAR), como 1 HOUR o 1 HOUR 30 MINUTE.",
            shown(&t)
        ))
    })?;
    if iv.iter().any(|(n, _)| *n == 0) {
        return Err(bad(format!("«{label}» tiene que ser mayor que cero.")));
    }
    Ok(Some(iv))
}

// ---------------------------------------------------------------------------
// The definition
// ---------------------------------------------------------------------------

/// A refreshable view's `CREATE` statement, part by part.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ViewDef {
    pub(crate) database: String,
    pub(crate) name: String,
    /// `EVERY` (aligned to the calendar) or `AFTER` (after the last one).
    pub(crate) every: bool,
    pub(crate) interval: Interval,
    pub(crate) offset: Option<Interval>,
    pub(crate) randomize: Option<Interval>,
    pub(crate) depends_on: Vec<(String, String)>,
    /// The refresh settings, name and value as written.
    pub(crate) settings: Vec<(String, String)>,
    pub(crate) append: bool,
    /// `TO db.table`; `None`: the view's own table (`ENGINE`).
    pub(crate) to: Option<(String, String)>,
    pub(crate) columns: Option<String>,
    /// The own table's engine and its clauses, without `ENGINE =`.
    pub(crate) engine: Option<String>,
    /// `EMPTY`: no refresh when it's created (a new view only).
    pub(crate) empty: bool,
    /// A user's name, or `CURRENT_USER`.
    pub(crate) definer: Option<String>,
    /// `DEFINER` or `NONE`.
    pub(crate) security: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) query: String,
}

/// An interval at the parser's position (`1 HOUR 30 MINUTE`).
fn interval_at(p: &mut Parser) -> Option<Interval> {
    let mut out = Vec::new();
    while p.kind_at(p.i) == Some(T::Word)
        && p.text_at(p.i).bytes().all(|b| b.is_ascii_digit())
        && p.kind_at(p.i + 1) == Some(T::Word)
    {
        let Some(unit) = Unit::parse(p.text_at(p.i + 1)) else {
            break;
        };
        out.push((p.text_at(p.i).parse().ok()?, unit));
        p.i += 2;
    }
    (!out.is_empty()).then_some(out)
}

/// The words that end a clause of the statement after the refresh clause.
fn ends_clause(p: &Parser, i: usize) -> bool {
    ["DEFINER", "COMMENT", "AS", "EMPTY", "POPULATE"]
        .iter()
        .any(|k| p.is_kw_at(i, k))
        || (p.is_kw_at(i, "SQL") && p.is_kw_at(i + 1, "SECURITY"))
}

/// Where the clause that starts at `from` ends (at depth 0).
fn clause_end(p: &Parser, from: usize) -> usize {
    let mut depth = 0i64;
    let mut i = from;
    while i < p.t.len() {
        if p.kind_at(i) == Some(T::Punct) {
            match p.text_at(i) {
                "(" | "[" => depth += 1,
                ")" | "]" => depth -= 1,
                _ => {}
            }
        } else if depth == 0 && ends_clause(p, i) {
            break;
        }
        i += 1;
    }
    i
}

/// The `)` that closes the `(` at `open`.
fn matching(p: &Parser, open: usize) -> Option<usize> {
    let mut depth = 0i64;
    for i in open..p.t.len() {
        if p.kind_at(i) == Some(T::Punct) {
            match p.text_at(i) {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// A setting's value: a number (maybe negative or with decimals), a word
/// or a string.
fn setting_value(p: &mut Parser) -> Option<String> {
    let from = p.i;
    p.eat_punct("-");
    match p.kind_at(p.i)? {
        T::Word | T::Str => p.i += 1,
        _ => return None,
    }
    if p.peek_punct(".") && p.kind_at(p.i + 1) == Some(T::Word) {
        p.i += 2;
    }
    Some(p.span(from, p.i).to_string())
}

/// Reads `SHOW CREATE TABLE` of a refreshable view.
pub(crate) fn parse_view(create: &str) -> Option<ViewDef> {
    let mut p = Parser::new(create)?;
    p.kw("CREATE")?;
    p.kw("MATERIALIZED")?;
    p.kw("VIEW")?;
    if p.eat_kw("IF") {
        p.kw("NOT")?;
        p.kw("EXISTS")?;
    }
    let (db, name) = p.qname()?;
    let mut d = ViewDef {
        database: db.unwrap_or_default(),
        name,
        ..ViewDef::default()
    };
    if p.eat_kw("ON") {
        p.kw("CLUSTER")?;
        p.part()?;
    }
    p.kw("REFRESH")?;
    d.every = if p.eat_kw("EVERY") {
        true
    } else {
        p.kw("AFTER")?;
        false
    };
    d.interval = interval_at(&mut p)?;
    loop {
        if p.eat_kw("OFFSET") {
            d.offset = Some(interval_at(&mut p)?);
        } else if p.peek_kw("RANDOMIZE") && p.is_kw_at(p.i + 1, "FOR") {
            p.i += 2;
            d.randomize = Some(interval_at(&mut p)?);
        } else if p.peek_kw("DEPENDS") && p.is_kw_at(p.i + 1, "ON") {
            p.i += 2;
            loop {
                let (db, n) = p.qname()?;
                d.depends_on
                    .push((db.unwrap_or_else(|| d.database.clone()), n));
                if !p.eat_punct(",") {
                    break;
                }
            }
        } else if p.eat_kw("SETTINGS") {
            loop {
                let k = p.part()?;
                if !p.eat_punct("=") {
                    return None;
                }
                let v = setting_value(&mut p)?;
                d.settings.push((k, v));
                if !p.eat_punct(",") {
                    break;
                }
            }
        } else if p.eat_kw("APPEND") {
            d.append = true;
        } else {
            break;
        }
    }
    loop {
        if p.done() {
            return None;
        }
        if p.eat_kw("TO") {
            if p.eat_kw("INNER") {
                p.kw("UUID")?;
                p.string()?;
            } else {
                let (db, n) = p.qname()?;
                d.to = Some((db.unwrap_or_else(|| d.database.clone()), n));
            }
        } else if p.peek_punct("(") {
            let close = matching(&p, p.i)?;
            d.columns = Some(lex::dedent(p.span(p.i + 1, close))).filter(|c| !c.is_empty());
            p.i = close + 1;
        } else if p.eat_kw("ENGINE") {
            p.eat_punct("=");
            let from = p.i;
            let to = clause_end(&p, from);
            d.engine = Some(lex::dedent(p.span(from, to))).filter(|e| !e.is_empty());
            p.i = to;
        } else if p.eat_kw("EMPTY") {
            d.empty = true;
        } else if p.eat_kw("POPULATE") {
        } else if p.eat_kw("DEFINER") {
            if !p.eat_punct("=") {
                return None;
            }
            d.definer = Some(p.part()?);
        } else if p.peek_kw("SQL") && p.is_kw_at(p.i + 1, "SECURITY") {
            p.i += 2;
            d.security = Some(p.part()?.to_ascii_uppercase());
        } else if p.eat_kw("COMMENT") {
            d.comment = Some(p.string()?);
        } else if p.eat_kw("AS") {
            // Before 24.x the comment came after the query.
            let n = p.t.len();
            if n >= p.i + 3 && p.is_kw_at(n - 2, "COMMENT") && p.kind_at(n - 1) == Some(T::Str) {
                d.comment = Some(lex::unquote(p.text_at(n - 1)));
                d.query = lex::trim_query(p.span(p.i, n - 2));
            } else {
                d.query = lex::trim_query(p.rest(p.i));
            }
            break;
        } else {
            return None;
        }
    }
    Some(d)
}

/// The app's id of a view or table.
fn qid(t: &(String, String)) -> String {
    job_id(&t.0, &t.1)
}

/// The name of a view or table in a statement.
fn sql_of(t: &(String, String)) -> String {
    qname(&t.0, &t.1)
}

fn user_sql(u: &str) -> String {
    if u == "CURRENT_USER" {
        u.to_string()
    } else {
        qi(u)
    }
}

/// The refresh clause without `REFRESH`: schedule, `OFFSET`, `RANDOMIZE
/// FOR`, `DEPENDS ON`, `SETTINGS`, `APPEND`.
pub(crate) fn refresh_sql(d: &ViewDef) -> String {
    let mut s = format!(
        "{} {}",
        if d.every { "EVERY" } else { "AFTER" },
        interval_text(&d.interval)
    );
    if let Some(o) = d.offset.as_ref().filter(|_| d.every) {
        s.push_str(&format!(" OFFSET {}", interval_text(o)));
    }
    if let Some(r) = &d.randomize {
        s.push_str(&format!(" RANDOMIZE FOR {}", interval_text(r)));
    }
    if !d.depends_on.is_empty() {
        let names: Vec<String> = d.depends_on.iter().map(sql_of).collect();
        s.push_str(&format!(" DEPENDS ON {}", names.join(", ")));
    }
    if !d.settings.is_empty() {
        let set: Vec<String> = d
            .settings
            .iter()
            .map(|(k, v)| format!("{k} = {v}"))
            .collect();
        s.push_str(&format!(" SETTINGS {}", set.join(", ")));
    }
    if d.append {
        s.push_str(" APPEND");
    }
    s
}

/// A refresh setting's value as the server reads it: `true` / `false` as
/// `1` / `0`.
fn setting_meaning(v: &str) -> String {
    let t = v.trim();
    if t.eq_ignore_ascii_case("true") {
        "1".into()
    } else if t.eq_ignore_ascii_case("false") {
        "0".into()
    } else {
        t.to_string()
    }
}

/// Whether the refresh clauses of `a` and `b` mean the same: the schedule,
/// `OFFSET` (with `EVERY`) and `RANDOMIZE FOR`, `DEPENDS ON` as a set, the
/// `SETTINGS` as name → value, `APPEND`. Their order and a setting's
/// spelling (`true` or `1`) don't count.
pub(crate) fn same_refresh(a: &ViewDef, b: &ViewDef) -> bool {
    let offset = |d: &ViewDef| d.offset.clone().filter(|_| d.every);
    let depends = |d: &ViewDef| d.depends_on.iter().cloned().collect::<BTreeSet<_>>();
    let settings = |d: &ViewDef| {
        d.settings
            .iter()
            .map(|(k, v)| (k.clone(), setting_meaning(v)))
            .collect::<BTreeMap<_, _>>()
    };
    a.every == b.every
        && a.interval == b.interval
        && offset(a) == offset(b)
        && a.randomize == b.randomize
        && depends(a) == depends(b)
        && settings(a) == settings(b)
        && a.append == b.append
}

/// `w` with its `DEPENDS ON` and `SETTINGS` in the order `cur` has them
/// (the new ones after), and a setting that means the same spelled as
/// `cur` has it.
pub(crate) fn in_server_order(cur: &ViewDef, w: &ViewDef) -> ViewDef {
    let mut d = w.clone();
    let mut depends: Vec<(String, String)> = cur
        .depends_on
        .iter()
        .filter(|x| w.depends_on.contains(x))
        .cloned()
        .collect();
    depends.extend(
        w.depends_on
            .iter()
            .filter(|x| !cur.depends_on.contains(x))
            .cloned(),
    );
    d.depends_on = depends;
    let mut settings: Vec<(String, String)> = Vec::new();
    for (k, v) in &cur.settings {
        if let Some((_, nv)) = w.settings.iter().find(|(n, _)| n == k) {
            let same = setting_meaning(v) == setting_meaning(nv);
            settings.push((k.clone(), if same { v.clone() } else { nv.clone() }));
        }
    }
    settings.extend(
        w.settings
            .iter()
            .filter(|(k, _)| !cur.settings.iter().any(|(n, _)| n == k))
            .cloned(),
    );
    d.settings = settings;
    d
}

/// `SQL SECURITY …` as `ALTER TABLE … MODIFY` takes it.
fn security_sql(d: &ViewDef) -> Option<String> {
    match d.security.as_deref()? {
        "DEFINER" => Some(match &d.definer {
            Some(u) => format!("SQL SECURITY DEFINER DEFINER = {}", user_sql(u)),
            None => "SQL SECURITY DEFINER".into(),
        }),
        "NONE" => Some("SQL SECURITY NONE".into()),
        _ => None,
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {}", l.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `CREATE [OR REPLACE] MATERIALIZED VIEW …`, each clause on its own line
/// (a `--` comment at the end of a fragment ends with its line). The
/// comment goes after the query, where every version takes it.
pub(crate) fn create_sql(d: &ViewDef, replace: bool, security: bool) -> String {
    let mut s = format!(
        "CREATE {}MATERIALIZED VIEW {}\nREFRESH {}",
        if replace { "OR REPLACE " } else { "" },
        qname(&d.database, &d.name),
        refresh_sql(d)
    );
    if let Some(t) = &d.to {
        s.push_str(&format!("\nTO {}", sql_of(t)));
    }
    if let Some(c) = &d.columns {
        s.push_str(&format!("\n(\n{}\n)", indent(c)));
    }
    if let Some(e) = d.engine.as_ref().filter(|_| d.to.is_none()) {
        s.push_str(&format!("\nENGINE = {e}"));
    }
    if d.empty {
        s.push_str("\nEMPTY");
    }
    if security {
        match d.security.as_deref() {
            Some("DEFINER") => {
                s.push('\n');
                if let Some(u) = &d.definer {
                    s.push_str(&format!("DEFINER = {} ", user_sql(u)));
                }
                s.push_str("SQL SECURITY DEFINER");
            }
            Some("NONE") => s.push_str("\nSQL SECURITY NONE"),
            _ => {}
        }
    }
    s.push_str(&format!("\nAS\n{}", d.query));
    if let Some(c) = &d.comment {
        s.push_str(&format!("\nCOMMENT {}", string_literal(c)));
    }
    s
}

// ---------------------------------------------------------------------------
// What the server has to pick from
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct Choices {
    /// Name and engine.
    pub(crate) databases: Vec<(String, String)>,
    pub(crate) refreshable: Vec<(String, String)>,
    /// Tables a view can write to.
    pub(crate) tables: Vec<(String, String)>,
    /// `None`: the user can't list them (no `SHOW USERS`).
    pub(crate) users: Option<Vec<String>>,
}

impl Choices {
    fn database_names(&self) -> Vec<String> {
        self.databases.iter().map(|(n, _)| n.clone()).collect()
    }

    /// The database takes `CREATE OR REPLACE`.
    pub(crate) fn replaceable(&self, db: &str) -> bool {
        self.databases
            .iter()
            .find(|(n, _)| n == db)
            .is_none_or(|(_, e)| matches!(e.as_str(), "Atomic" | "Replicated" | "Shared"))
    }
}

const SYSTEM_DBS: &str = "('system', 'INFORMATION_SCHEMA', 'information_schema')";

impl ClickHouseSession {
    pub(crate) async fn view_choices(&self) -> Result<Choices> {
        let pairs = |rows: Vec<Vec<Value>>| -> Vec<(String, String)> {
            rows.iter()
                .filter_map(|r| Some((jobs::opt(r.first())?, jobs::opt(r.get(1))?)))
                .collect()
        };
        let databases = pairs(
            self.rows(
                &format!(
                    "SELECT name, engine FROM system.databases WHERE name NOT IN {SYSTEM_DBS} ORDER BY name"
                ),
                &[],
            )
            .await?,
        );
        let refreshable = pairs(
            self.rows(
                "SELECT database, view FROM system.view_refreshes ORDER BY database, view",
                &[],
            )
            .await?,
        );
        let tables = pairs(
            self.rows(
                &format!(
                    "SELECT database, name FROM system.tables
                     WHERE database NOT IN {SYSTEM_DBS} AND NOT is_temporary AND name NOT LIKE '.inner%'
                       AND engine NOT IN ('View', 'MaterializedView', 'LiveView', 'WindowView', 'Dictionary')
                     ORDER BY database, name LIMIT 20000"
                ),
                &[],
            )
            .await?,
        );
        let users = self
            .rows("SELECT name FROM system.users ORDER BY name", &[])
            .await
            .ok()
            .map(|rows| rows.iter().filter_map(|r| jobs::opt(r.first())).collect());
        Ok(Choices {
            databases,
            refreshable,
            tables,
            users,
        })
    }

    /// A view as the server has it, and whether it's started.
    pub(crate) async fn view_current(&self, id: &str) -> Result<(ViewDef, bool)> {
        let (db, name) = ids(id)?;
        let r = self.refresh(&db, &name).await?;
        let rows = self
            .rows(&format!("SHOW CREATE TABLE {}", job_id(&db, &name)), &[])
            .await?;
        let create = rows
            .first()
            .and_then(|r| r.first())
            .map(crate::text)
            .unwrap_or_default();
        let mut d = parse_view(&create).ok_or_else(|| {
            Error::Query(format!(
                "no se pudo leer la definición de la vista refrescable {db}.{name}"
            ))
        })?;
        d.database = db;
        d.name = name;
        Ok((d, r.enabled()))
    }

    async fn table_exists(&self, db: &str, name: &str) -> Result<bool> {
        let rows = self
            .rows(
                "SELECT count() FROM system.tables WHERE database = {db:String} AND name = {n:String}",
                &[("db", db), ("n", name)],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| jobs::num(r.first()))
            .unwrap_or(0.0)
            > 0.0)
    }
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn setting<'a>(d: &'a ViewDef, k: &str) -> Option<&'a str> {
    d.settings
        .iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
}

/// A number setting as the field shows it.
fn number_value(d: &ViewDef, k: &str) -> Value {
    match setting(d, k) {
        None => Value::Null,
        Some(v) => v.parse::<i64>().map_or_else(|_| json!(v), |n| json!(n)),
    }
}

/// The settings without a field of their own, as `name = value, …`.
fn other_settings_text(d: &ViewDef) -> String {
    d.settings
        .iter()
        .filter(|(k, _)| !KNOWN_SETTINGS.contains(&k.as_str()))
        .map(|(k, v)| format!("{k} = {v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn values_of(d: &ViewDef, enabled: bool, existing: bool) -> BTreeMap<String, Value> {
    let ivt = |iv: &Option<Interval>| iv.as_deref().map(interval_text).unwrap_or_default();
    let mut m: BTreeMap<String, Value> = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_string(), v);
    };
    put("database", json!(d.database));
    put("name", json!(d.name));
    put("enabled", json!(enabled));
    put("comment", json!(d.comment.clone().unwrap_or_default()));
    put("mode", json!(if d.every { "every" } else { "after" }));
    put("interval", json!(interval_text(&d.interval)));
    put("offset", json!(ivt(&d.offset)));
    put("randomize", json!(ivt(&d.randomize)));
    put(
        "depends_on",
        json!(d.depends_on.iter().map(qid).collect::<Vec<_>>()),
    );
    put(RETRIES, number_value(d, RETRIES));
    put(BACKOFF, number_value(d, BACKOFF));
    put(MAX_BACKOFF, number_value(d, MAX_BACKOFF));
    put(
        ALL_REPLICAS,
        json!(setting(d, ALL_REPLICAS).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))),
    );
    put("other_settings", json!(other_settings_text(d)));
    put(
        "target_mode",
        json!(if d.to.is_some() { "to" } else { "inner" }),
    );
    put("target", json!(d.to.as_ref().map(qid).unwrap_or_default()));
    put("engine", json!(d.engine.clone().unwrap_or_default()));
    put("columns", json!(d.columns.clone().unwrap_or_default()));
    put("append", json!(d.append));
    if !existing {
        put("empty", json!(d.empty));
    }
    put(
        "sql_security",
        json!(match d.security.as_deref() {
            Some("NONE") => "none",
            Some(_) => "definer",
            None => "",
        }),
    );
    put("definer", json!(d.definer.clone().unwrap_or_default()));
    put("query", json!(d.query));
    m
}

pub(crate) fn build_form(
    d: &ViewDef,
    enabled: bool,
    existing: bool,
    c: &Ctx,
    ch: &Choices,
) -> ObjectForm {
    let mut f = ObjectForm {
        values: values_of(d, enabled, existing),
        ..ObjectForm::default()
    };
    let general = FormSection::new("general", "General").fields(vec![
        FormField::new("database", "Base de datos", FormKind::reference("database", false))
            .required(),
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("enabled", "Habilitada", FormKind::Bool)
            .default(json!(true))
            .help("SYSTEM START VIEW / SYSTEM STOP VIEW. Deshabilitarla no sobrevive a un reinicio del servidor: al arrancar vuelve a refrescar."),
        FormField::new("comment", "Comentario", FormKind::Textarea),
    ]);
    let schedule = FormSection::new("schedule", "Programación").fields(vec![
        FormField::new(
            "mode",
            "Cuándo refresca",
            FormKind::select(&[
                ("every", "Cada intervalo, alineado al calendario (EVERY)"),
                ("after", "Un intervalo después del refresco anterior (AFTER)"),
            ]),
        )
        .required()
        .default(json!("every")),
        FormField::new("interval", "Intervalo", FormKind::Text)
            .required()
            .placeholder("1 HOUR")
            .help("Un número y una unidad (SECOND, MINUTE, HOUR, DAY, WEEK, MONTH o YEAR); se pueden sumar varias, como 1 HOUR 30 MINUTE, sin mezclar meses o años con unidades más chicas.")
            .default(json!("1 HOUR")),
        FormField::new("offset", "Desplazamiento (OFFSET)", FormKind::Text)
            .placeholder("2 HOUR")
            .help("Cuánto después del comienzo de cada período refresca (EVERY 1 DAY OFFSET 2 HOUR: a las 2:00); menor que el intervalo.")
            .when("mode", &["every"]),
        FormField::new("randomize", "Variación al azar (RANDOMIZE FOR)", FormKind::Text)
            .placeholder("10 MINUTE")
            .help("Mueve cada refresco un tiempo al azar dentro de ese margen, para que muchas vistas no refresquen a la vez."),
        FormField::new(
            "depends_on",
            "Depende de (DEPENDS ON)",
            FormKind::reference("refreshable", true),
        )
        .help("Refresca después de que se refresquen esas vistas."),
    ]);
    let retries = FormSection::new("retries", "Reintentos").fields(vec![
        FormField::new(RETRIES, "Reintentos", FormKind::number(Some(-1.0), None))
            .help("Cuántas veces reintenta un refresco que falla (-1: sin límite). Vacío: el valor del servidor."),
        FormField::new(
            BACKOFF,
            "Espera inicial entre reintentos (ms)",
            FormKind::number(Some(0.0), None),
        )
        .help("Vacío: el valor del servidor."),
        FormField::new(
            MAX_BACKOFF,
            "Espera máxima entre reintentos (ms)",
            FormKind::number(Some(0.0), None),
        )
        .help("Vacío: el valor del servidor."),
        FormField::new(ALL_REPLICAS, "Refrescar en todas las réplicas", FormKind::Bool)
            .help("En una base de datos Replicated, refresca en cada réplica y no en una sola."),
        FormField::new("other_settings", "Otros ajustes", FormKind::Text)
            .placeholder("nombre = valor, nombre = valor")
            .help("Ajustes del refresco sin campo propio, como los escribe ClickHouse."),
    ]);
    let mut target_fields = vec![
        FormField::new(
            "target_mode",
            "Dónde guarda las filas",
            FormKind::select(&[
                ("inner", "En una tabla propia de la vista (ENGINE)"),
                ("to", "En una tabla existente (TO)"),
            ]),
        )
        .required()
        .default(json!("inner")),
        FormField::new("target", "Tabla destino", FormKind::reference("table", false))
            .when("target_mode", &["to"]),
        FormField::new("engine", "Motor", FormKind::Textarea)
            .placeholder("MergeTree ORDER BY tuple()")
            .help("El motor de la tabla propia, con sus cláusulas (ORDER BY, PARTITION BY, TTL, SETTINGS…). Vacío: el motor predeterminado del servidor.")
            .when("target_mode", &["inner"]),
        FormField::new("columns", "Columnas", FormKind::Textarea)
            .placeholder("`c` UInt64, `fecha` Date")
            .help("Opcional: las columnas con sus tipos; vacío: las de la consulta."),
        FormField::new("append", "Agregar las filas (APPEND)", FormKind::Bool).help(
            "Sin APPEND, cada refresco reemplaza el contenido entero; con APPEND, agrega lo que devuelve la consulta.",
        ),
    ];
    if !existing {
        target_fields.push(
            FormField::new("empty", "Crear sin refrescar (EMPTY)", FormKind::Bool)
                .help("No refresca al crearla: el primer refresco es el programado."),
        );
    }
    let target = FormSection::new("target", "Destino").fields(target_fields);
    let mut sections = vec![general, schedule, retries, target];
    if c.sql_security() {
        sections.push(FormSection::new("security", "Seguridad").fields(vec![
            FormField::new(
                "sql_security",
                "Permisos de la consulta (SQL SECURITY)",
                FormKind::select(&[
                    ("definer", "Los del definidor (DEFINER)"),
                    ("none", "Sin comprobar permisos (NONE)"),
                ]),
            )
            .default(json!("definer")),
            FormField::new("definer", "Definidor", FormKind::reference("user", false))
                .help("El usuario con cuyos permisos corre la consulta de cada refresco.")
                .when("sql_security", &["definer"]),
        ]));
    }
    sections.push(
        FormSection::new("query", "Consulta").field(
            FormField::new("query", "Consulta", FormKind::code("sql"))
                .required()
                .help("La consulta SELECT que corre cada refresco."),
        ),
    );
    f.sections = sections;

    let own = (d.database.clone(), d.name.clone());
    let mut others: Vec<(String, String)> = ch
        .refreshable
        .iter()
        .filter(|p| !existing || **p != own)
        .cloned()
        .collect();
    for p in &d.depends_on {
        if !others.contains(p) {
            others.push(p.clone());
        }
    }
    f.choices
        .insert("database".into(), same_pairs(&ch.database_names()));
    f.choices.insert("refreshable".into(), id_pairs(&others));
    let mut tables = ch.tables.clone();
    if let Some(t) = d.to.as_ref().filter(|t| !tables.contains(t)) {
        tables.push(t.clone());
    }
    f.choices.insert("table".into(), id_pairs(&tables));
    let mut users = vec![("CURRENT_USER".to_string(), "CURRENT_USER".to_string())];
    users.extend(same_pairs(ch.users.as_deref().unwrap_or_default()));
    if let Some(u) = d
        .definer
        .as_ref()
        .filter(|u| !users.iter().any(|(v, _)| v == *u))
    {
        users.push((u.clone(), u.clone()));
    }
    f.choices.insert("user".into(), users);

    if c.experimental() {
        f.notes.push(super::EXPERIMENTAL.to_string());
    }
    if existing {
        f.notes.push(if ch.replaceable(&d.database) {
            RECREATE_NOTE.to_string()
        } else {
            NO_REPLACE_NOTE.to_string()
        });
        if d.to.is_none() {
            f.notes.push(MOVE_NOTE.to_string());
        }
    }
    f
}

pub(crate) async fn form(s: &ClickHouseSession, c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let ch = s.view_choices().await?;
    match id {
        None => {
            let names = ch.database_names();
            let database = if names.contains(&s.database) {
                s.database.clone()
            } else {
                names.first().cloned().unwrap_or_else(|| "default".into())
            };
            let d = ViewDef {
                database,
                every: true,
                interval: vec![(1, Unit::Hour)],
                security: c.sql_security().then(|| "DEFINER".to_string()),
                definer: c.sql_security().then(|| "CURRENT_USER".to_string()),
                ..ViewDef::default()
            };
            Ok(build_form(&d, true, false, c, &ch))
        }
        Some(id) => {
            let (d, enabled) = s.view_current(id).await?;
            Ok(build_form(&d, enabled, true, c, &ch))
        }
    }
}

// ---------------------------------------------------------------------------
// From the designer's values
// ---------------------------------------------------------------------------

/// A fragment of SQL from the form (an engine clause, a column list):
/// `None` when empty; refused when it isn't closed.
fn fragment(v: Vals, k: &str, label: &str) -> Result<Option<String>> {
    let Some(t) = v.opt(k) else { return Ok(None) };
    if !lex::closed(&t) {
        return Err(bad(format!(
            "«{label}»: tiene un texto, un nombre o un comentario sin cerrar, paréntesis que no cierran o un «;»."
        )));
    }
    Ok(Some(t))
}

fn setting_name_ok(k: &str) -> bool {
    let mut cs = k.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `name = value, …`.
pub(crate) fn other_settings(text: &str) -> Result<Vec<(String, String)>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let err = || {
        bad(format!(
            "«Otros ajustes»: «{}» no se entiende: escribilos como nombre = valor, separados por comas.",
            shown(text)
        ))
    };
    let mut p = Parser::new(text).ok_or_else(err)?;
    let mut out: Vec<(String, String)> = Vec::new();
    loop {
        if p.kind_at(p.i) != Some(T::Word) {
            return Err(err());
        }
        let k = p.text_at(p.i).to_string();
        p.i += 1;
        if !setting_name_ok(&k) || !p.eat_punct("=") {
            return Err(err());
        }
        if KNOWN_SETTINGS.contains(&k.as_str()) {
            return Err(bad(format!("«Otros ajustes»: {k} tiene su propio campo.")));
        }
        let v = setting_value(&mut p).ok_or_else(err)?;
        if out.iter().any(|(n, _)| *n == k) {
            return Err(bad(format!("«Otros ajustes»: {k} está dos veces.")));
        }
        out.push((k, v));
        if p.done() {
            break;
        }
        if !p.eat_punct(",") {
            return Err(err());
        }
    }
    Ok(out)
}

fn check_user(u: String, ch: &Choices, current: Option<&ViewDef>) -> Result<String> {
    if u == "CURRENT_USER" || current.and_then(|c| c.definer.as_deref()) == Some(u.as_str()) {
        return Ok(u);
    }
    match &ch.users {
        Some(users) if users.contains(&u) => Ok(u),
        None if !u.chars().any(char::is_control) => Ok(u),
        _ => Err(bad(format!(
            "«Definidor»: el usuario «{}» no existe en este servidor.",
            shown(&u)
        ))),
    }
}

/// The view the values describe, checked, and whether it should be
/// started. `current`: the view being edited.
pub(crate) fn wanted(
    values: &FormValues,
    c: &Ctx,
    ch: &Choices,
    current: Option<&ViewDef>,
) -> Result<(ViewDef, bool)> {
    let v = Vals(&values.values);
    let database = database_field(v, &ch.database_names())?;
    let name = name_field(v, "name", "Nombre")?;
    let every = match v.text("mode").as_str() {
        "" | "every" => true,
        "after" => false,
        other => {
            return Err(bad(format!(
                "«{}» no es un modo de refresco (EVERY o AFTER).",
                shown(other)
            )))
        }
    };
    let interval = interval_field(v, "interval", "Intervalo")?
        .ok_or_else(|| bad("«Intervalo» es obligatorio.".into()))?;
    let Some(period) = total(&interval) else {
        return Err(bad(
            "«Intervalo»: ClickHouse no mezcla meses o años con unidades más chicas en un intervalo."
                .into(),
        ));
    };
    let offset = if every {
        interval_field(v, "offset", "Desplazamiento (OFFSET)")?
    } else {
        None
    };
    if let Some(o) = offset.as_deref().and_then(total) {
        if o.0 == period.0 && o.1 >= period.1 {
            return Err(bad(
                "«Desplazamiento (OFFSET)» tiene que ser menor que el intervalo.".into(),
            ));
        }
    }
    let randomize = interval_field(v, "randomize", "Variación al azar (RANDOMIZE FOR)")?;

    let own: Vec<(String, String)> = std::iter::once((database.clone(), name.clone()))
        .chain(current.map(|d| (d.database.clone(), d.name.clone())))
        .collect();
    let mut depends_on: Vec<(String, String)> = Vec::new();
    for id in v.strings("depends_on") {
        // One it already names stays allowed after that view is gone.
        let pair = parse_id(&id)
            .filter(|p| {
                ch.refreshable.contains(p) || current.is_some_and(|d| d.depends_on.contains(p))
            })
            .ok_or_else(|| {
                bad(format!(
                    "«Depende de»: «{}» no es una vista refrescable de este servidor.",
                    shown(&id)
                ))
            })?;
        if own.contains(&pair) {
            return Err(bad(
                "«Depende de»: una vista no puede depender de sí misma.".into(),
            ));
        }
        if !depends_on.contains(&pair) {
            depends_on.push(pair);
        }
    }

    let mut settings: Vec<(String, String)> = Vec::new();
    if let Some(n) = v.int(RETRIES, "Reintentos", -1, 1_000_000)? {
        settings.push((RETRIES.into(), n.to_string()));
    }
    let max_ms = i64::from(u32::MAX);
    if let Some(n) = v.int(BACKOFF, "Espera inicial entre reintentos (ms)", 0, max_ms)? {
        settings.push((BACKOFF.into(), n.to_string()));
    }
    if let Some(n) = v.int(
        MAX_BACKOFF,
        "Espera máxima entre reintentos (ms)",
        0,
        max_ms,
    )? {
        settings.push((MAX_BACKOFF.into(), n.to_string()));
    }
    if v.bool_or(ALL_REPLICAS, false) {
        settings.push((ALL_REPLICAS.into(), "1".into()));
    }
    settings.extend(other_settings(&v.text("other_settings"))?);

    let to = match v.text("target_mode").as_str() {
        "" | "inner" => None,
        "to" => {
            let id = v
                .opt("target")
                .ok_or_else(|| bad("«Tabla destino» es obligatorio.".into()))?;
            let pair = parse_id(&id)
                .filter(|p| ch.tables.contains(p) || current.and_then(|d| d.to.as_ref()) == Some(p))
                .ok_or_else(|| {
                    bad(format!(
                        "«Tabla destino»: «{}» no es una tabla de este servidor.",
                        shown(&id)
                    ))
                })?;
            Some(pair)
        }
        other => {
            return Err(bad(format!(
                "«{}» no es un destino (ENGINE o TO).",
                shown(other)
            )))
        }
    };
    let engine = if to.is_none() {
        fragment(v, "engine", "Motor")?
    } else {
        None
    };
    let columns = fragment(v, "columns", "Columnas")?;

    let (security, definer) = if c.sql_security() {
        match v.text("sql_security").as_str() {
            "" => (None, None),
            "none" => (Some("NONE".to_string()), None),
            "definer" => {
                let definer = match v.opt("definer") {
                    None => None,
                    Some(u) => Some(check_user(u, ch, current)?),
                };
                (Some("DEFINER".to_string()), definer)
            }
            other => {
                return Err(bad(format!(
                    "«{}» no es un modo de SQL SECURITY (DEFINER o NONE).",
                    shown(other)
                )))
            }
        }
    } else {
        (None, None)
    };

    let d = ViewDef {
        database,
        name,
        every,
        interval,
        offset,
        randomize,
        depends_on,
        settings,
        append: v.bool_or("append", false),
        to,
        columns,
        engine,
        empty: current.is_none() && v.bool_or("empty", false),
        definer,
        security,
        comment: v.opt("comment"),
        query: query_field(v)?,
    };
    Ok((d, v.bool_or("enabled", true)))
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

fn checked(mut p: Plan) -> Result<Plan> {
    let restore = p.restore.iter().flat_map(|r| &r.statements);
    for s in p.statements.iter().chain(restore) {
        one_statement(s)?;
    }
    if p.statements.is_empty() {
        p.settings.clear();
    }
    Ok(p)
}

fn label(d: &ViewDef) -> String {
    format!("{}.{}", d.database, d.name)
}

pub(crate) fn plan_create(w: &ViewDef, enabled: bool, c: &Ctx) -> Result<Plan> {
    let mut p = Plan {
        comments: vec![format!("Crea la vista refrescable {}.", label(w))],
        settings: c.settings(),
        statements: vec![create_sql(w, false, c.sql_security())],
        database: Some(w.database.clone()),
        id: job_id(&w.database, &w.name),
        restore: None,
    };
    if !enabled {
        p.statements
            .push(format!("SYSTEM STOP VIEW {}", qname(&w.database, &w.name)));
        p.comments.push(STOP_NOT_KEPT.into());
    }
    checked(p)
}

/// The change from `cur` (started or not) to `w`. `replaceable`: the
/// database takes `CREATE OR REPLACE`.
pub(crate) fn plan_edit(
    cur: &ViewDef,
    cur_enabled: bool,
    w: &ViewDef,
    enabled: bool,
    c: &Ctx,
    replaceable: bool,
) -> Result<Plan> {
    // What the clauses list keeps the server's order.
    let w = &in_server_order(cur, w);
    let old_id = qname(&cur.database, &cur.name);
    let new_id = qname(&w.database, &w.name);
    let mut p = Plan {
        comments: vec![format!("Cambia la vista refrescable {}.", label(cur))],
        settings: c.settings(),
        statements: Vec::new(),
        database: None,
        id: job_id(&w.database, &w.name),
        restore: None,
    };
    // ClickHouse doesn't move a view with its own table to another
    // database: it's created there and the old one dropped.
    if cur.database != w.database && w.to.is_none() {
        p.comments.push(format!(
            "ClickHouse no mueve a otra base de datos una vista con tabla propia: se crea {} con la definición nueva y se borra {}.",
            label(w),
            label(cur)
        ));
        p.comments.push(
            "Se pierde su estado de refresco, y su tabla propia nueva empieza vacía: la llena el primer refresco.".into(),
        );
        p.comments.push(DEPENDS_NOTE.into());
        let mut d = w.clone();
        d.empty = false;
        p.database = Some(w.database.clone());
        p.statements.push(create_sql(&d, false, c.sql_security()));
        if !enabled {
            p.statements.push(format!("SYSTEM STOP VIEW {new_id}"));
            p.comments.push(STOP_NOT_KEPT.into());
        }
        p.statements.push(format!("DROP VIEW {old_id}"));
        return checked(p);
    }
    let same = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => lex::same_sql(a, b),
        _ => false,
    };
    let engine_of = |d: &ViewDef| d.engine.clone().filter(|_| d.to.is_none());
    let recreate = cur.to != w.to
        || !same(&engine_of(cur), &engine_of(w))
        || !same(&cur.columns, &w.columns)
        || cur.append != w.append;
    if recreate {
        p.comments.push("ClickHouse no cambia en su lugar el destino, el motor, las columnas ni APPEND de una vista: se vuelve a crear con la definición nueva.".into());
        p.comments.push(if w.to.is_none() {
            "Se pierde su estado de refresco (el último y el próximo), y su tabla propia se vuelve a crear vacía: la llena el primer refresco.".into()
        } else {
            "Se pierde su estado de refresco (el último y el próximo); la tabla destino conserva sus filas.".into()
        });
        let mut d = w.clone();
        d.database = cur.database.clone();
        d.name = cur.name.clone();
        d.empty = false;
        p.database = Some(cur.database.clone());
        if replaceable {
            p.statements.push(create_sql(&d, true, c.sql_security()));
        } else {
            p.comments.push(NO_REPLACE_NOTE.into());
            p.comments.push(format!(
                "Si el servidor rechaza la vista nueva, DBine vuelve a crear {} como estaba.",
                label(cur)
            ));
            let drop = p.statements.len();
            p.statements.push(format!("DROP VIEW {old_id}"));
            p.statements.push(create_sql(&d, false, c.sql_security()));
            // The old view, as it was (stopped, when it was).
            let mut old = cur.clone();
            old.empty = false;
            let mut statements = vec![create_sql(&old, false, c.sql_security())];
            if !cur_enabled {
                statements.push(format!("SYSTEM STOP VIEW {old_id}"));
            }
            p.restore = Some(Restore {
                drop,
                create: drop + 1,
                statements,
                what: label(cur),
            });
        }
    } else {
        if !lex::same_sql(&cur.query, &w.query) {
            p.database = Some(cur.database.clone());
            p.statements
                .push(format!("ALTER TABLE {old_id} MODIFY QUERY\n{}", w.query));
        }
        // Only a clause that means something else: MODIFY REFRESH
        // reschedules the view.
        if !same_refresh(cur, w) {
            p.statements.push(format!(
                "ALTER TABLE {old_id} MODIFY REFRESH {}",
                refresh_sql(w)
            ));
        }
        if c.sql_security()
            && w.security.is_some()
            && (&cur.security, &cur.definer) != (&w.security, &w.definer)
        {
            if let Some(sec) = security_sql(w) {
                p.statements
                    .push(format!("ALTER TABLE {old_id} MODIFY {sec}"));
            }
        }
        if cur.comment.as_deref().unwrap_or_default() != w.comment.as_deref().unwrap_or_default() {
            p.statements.push(format!(
                "ALTER TABLE {old_id} MODIFY COMMENT {}",
                string_literal(w.comment.as_deref().unwrap_or_default())
            ));
        }
    }
    if (&cur.database, &cur.name) != (&w.database, &w.name) {
        p.statements
            .push(format!("RENAME TABLE {old_id} TO {new_id}"));
        p.comments.push(DEPENDS_NOTE.into());
    }
    // A view created again starts refreshing.
    let started = recreate || cur_enabled;
    if enabled != started {
        let verb = if enabled { "START" } else { "STOP" };
        p.statements.push(format!("SYSTEM {verb} VIEW {new_id}"));
        if !enabled {
            p.comments.push(STOP_NOT_KEPT.into());
        }
    }
    if p.statements.is_empty() {
        p.comments = vec![format!("{}: no hay cambios que guardar.", label(cur))];
    }
    checked(p)
}

pub(crate) fn drop_plan(cur: &ViewDef) -> Plan {
    let name = qname(&cur.database, &cur.name);
    Plan {
        comments: vec![
            format!("Borra la vista refrescable {}.", label(cur)),
            match &cur.to {
                Some((db, t)) => format!("La tabla destino {db}.{t} queda, con sus filas."),
                None => "Su tabla propia se borra con ella, con sus datos.".into(),
            },
        ],
        settings: Vec::new(),
        statements: vec![format!("DROP VIEW {name}")],
        database: None,
        id: job_id(&cur.database, &cur.name),
        restore: None,
    }
}

pub(crate) async fn plan(
    s: &ClickHouseSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let ch = s.view_choices().await?;
    match id {
        None => {
            let (w, enabled) = wanted(values, c, &ch, None)?;
            plan_create(&w, enabled, c)
        }
        Some(id) => {
            let (cur, cur_enabled) = s.view_current(id).await?;
            let (w, enabled) = wanted(values, c, &ch, Some(&cur))?;
            if (&w.database, &w.name) != (&cur.database, &cur.name)
                && s.table_exists(&w.database, &w.name).await?
            {
                return Err(bad(format!(
                    "Ya existe una tabla o una vista {}.{}.",
                    w.database, w.name
                )));
            }
            plan_edit(
                &cur,
                cur_enabled,
                &w,
                enabled,
                c,
                ch.replaceable(&cur.database),
            )
        }
    }
}

pub(crate) async fn delete_plan(s: &ClickHouseSession, _c: &Ctx, id: &str) -> Result<Plan> {
    let (cur, _) = s.view_current(id).await?;
    Ok(drop_plan(&cur))
}
