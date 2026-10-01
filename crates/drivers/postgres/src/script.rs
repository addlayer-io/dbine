//! Editor scripts the way psql runs them: the app splits the script with
//! PostgreSQL's lexer (dollar quotes, `E'…'`, nested comments, `BEGIN
//! ATOMIC` bodies) and hands `execute` one statement at a time, so each
//! one is its own simple query. CREATE DATABASE, VACUUM or CREATE INDEX
//! CONCURRENTLY then work in a script, and a failed statement no longer
//! rolls back the ones before it behind their "rows affected".
//!
//! Here: the statement's command tag (tokio-postgres keeps only the
//! count), server errors with their SQLSTATE and position, notices, and
//! the transaction the session is in (manual mode sends the `BEGIN` psql
//! sends with `AUTOCOMMIT off`).

use crate::{db_text, err, Variant};
use dbine_driver::sql::{self, ScriptDialect};
use dbine_driver::{Error, Message, MessageLevel, QueryOutcome, ScriptError, TxState};
use tokio_postgres::error::{DbError, ErrorPosition, Severity, SqlState};

/// psql's lexer.
pub(crate) const DIALECT: ScriptDialect = ScriptDialect::postgres();

/// In front of the describe that gets a query's column types (see
/// `PgSession::run_statement`), so monitors can tell it from the query;
/// the profiler leaves it out.
pub(crate) const DESCRIBE: &str = "/* dbine:describe */ ";

impl Variant {
    /// Autocommit off, Commit and Rollback from the editor. Not on the
    /// engines whose transactions can't hold what an editor runs:
    /// Materialize (no DDL, only reads or only inserts), RisingWave (read
    /// only), CrateDB (BEGIN and COMMIT are accepted and ignored) and
    /// Denodo (its transactions span the data sources behind it).
    pub(crate) fn manual_transactions(self) -> bool {
        !matches!(self, Variant::Materialize | Variant::RisingWave | Variant::CrateDb | Variant::Denodo)
    }

    /// Engines that say whether a transaction block is open: `now() <>
    /// statement_timestamp()` (failing with 25P02 in an aborted one), or
    /// CockroachDB's `SHOW TRANSACTION STATUS`. The others keep the state
    /// DBine tracks from the statements it runs.
    pub(crate) fn probes_transaction(self) -> bool {
        self.plpgsql() || self.mpp() || matches!(self, Variant::Cockroach | Variant::Yellowbrick)
    }

    /// A failed statement aborts the open transaction until ROLLBACK
    /// (PostgreSQL and the engines on its executor). H2 goes on.
    pub(crate) fn aborts_on_error(self) -> bool {
        self != Variant::H2
    }

    /// The reference tool's default after a failed statement: psql goes on
    /// (ON_ERROR_STOP off), and so do ysqlsh and the tools of the engines
    /// that use psql; `cockroach sql` stops on errors when it runs a file.
    pub(crate) fn continue_on_error(self) -> bool {
        self != Variant::Cockroach
    }
}

/// The first words of a statement, lowercase, comments left out: enough
/// to name it.
pub(crate) fn head(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for w in sql::strip_comments(text, &DIALECT, false).split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if !w.is_empty() {
            out.push(w.to_ascii_lowercase());
            if out.len() == 6 {
                break;
            }
        }
    }
    out
}

/// What a statement runs: its first word, or for `WITH …` the verb after
/// its CTEs (select, insert, update, delete, merge).
pub(crate) fn verb(text: &str, head: &[String]) -> String {
    match head.first().map(String::as_str) {
        Some("with") => main_verb(text).unwrap_or_else(|| "select".into()),
        Some(w) => w.to_string(),
        None => String::new(),
    }
}

/// The first verb outside parentheses and quotes of a `WITH …` statement.
fn main_verb(text: &str) -> Option<String> {
    let t = sql::strip_comments(text, &DIALECT, false);
    let b = t.as_bytes();
    let (mut depth, mut i) = (0i32, 0);
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'$' => {
                // A dollar-quoted string: skip to its closing tag.
                let tag_end = t[i + 1..].find('$').map(|e| i + 1 + e);
                if let Some(e) = tag_end.filter(|&e| t[i + 1..e].bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')) {
                    let tag = &t[i..=e];
                    match t[e + 1..].find(tag) {
                        Some(close) => i = e + 1 + close + tag.len() - 1,
                        None => return None,
                    }
                }
            }
            c if depth == 0 && (c.is_ascii_alphabetic() || c == b'_') => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                let w = t[s..i].to_ascii_lowercase();
                if matches!(w.as_str(), "select" | "insert" | "update" | "delete" | "merge" | "values" | "table") {
                    return Some(w);
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The command tag psql prints for a statement ("INSERT 0 3", "CREATE
/// TABLE", "SELECT 5"), rebuilt from its words and the count, since the
/// client library keeps only the count. `None` when the words don't say
/// (EXECUTE of a prepared statement takes its statement's tag).
pub(crate) fn tag(head: &[String], verb: &str, n: u64) -> Option<String> {
    let w = |i: usize| head.get(i).map(String::as_str).unwrap_or("");
    Some(match verb {
        "insert" => format!("INSERT 0 {n}"),
        "update" | "delete" | "merge" | "fetch" | "move" | "copy" => format!("{} {n}", verb.to_ascii_uppercase()),
        "select" | "values" | "table" => format!("SELECT {n}"),
        "begin" => "BEGIN".into(),
        "start" => "START TRANSACTION".into(),
        "end" => "COMMIT".into(),
        "abort" => "ROLLBACK".into(),
        "commit" | "rollback" if w(1) == "prepared" => format!("{} PREPARED", verb.to_ascii_uppercase()),
        "prepare" if w(1) == "transaction" => "PREPARE TRANSACTION".into(),
        "truncate" => "TRUNCATE TABLE".into(),
        "lock" => "LOCK TABLE".into(),
        "create" | "alter" | "drop" => {
            let kind = object_kind(&head[1.min(head.len())..])?;
            format!("{} {kind}", verb.to_ascii_uppercase())
        }
        "execute" | "" => return None,
        v => v.to_ascii_uppercase(),
    })
}

/// The object a CREATE / ALTER / DROP names, as its tag spells it
/// ("TABLE", "MATERIALIZED VIEW", "INDEX" for CREATE UNIQUE INDEX…).
fn object_kind(words: &[String]) -> Option<String> {
    const MODIFIERS: [&str; 12] =
        ["or", "replace", "unique", "temp", "temporary", "unlogged", "global", "local", "recursive", "trusted", "procedural", "constraint"];
    let rest: Vec<&str> = words.iter().map(String::as_str).skip_while(|w| MODIFIERS.contains(w)).collect();
    let n = match rest.as_slice() {
        ["foreign", "data", "wrapper", ..] | ["text", "search", _, ..] => 3,
        ["materialized" | "foreign" | "event" | "access" | "default", _, ..] | ["user", "mapping", ..] => 2,
        ["operator", "class" | "family", ..] => 2,
        [_, ..] => 1,
        [] => return None,
    };
    Some(rest[..n].join(" ").to_ascii_uppercase())
}

/// Rows the statement touched or read, when its tag carries a count (DDL
/// shows its tag alone; CREATE TABLE … AS reports the rows it wrote).
pub(crate) fn affected(verb: &str, n: u64) -> Option<u64> {
    let counted = matches!(verb, "insert" | "update" | "delete" | "merge" | "fetch" | "move" | "copy" | "select" | "values" | "table");
    (counted || n > 0).then_some(n)
}

/// Statements psql never wraps in its implicit `BEGIN` with AUTOCOMMIT
/// off: transaction control, and what can't run in a transaction block.
pub(crate) fn no_begin(v: Variant, head: &[String]) -> bool {
    let w: Vec<&str> = head.iter().map(String::as_str).collect();
    match w.as_slice() {
        [] => true,
        ["begin" | "start" | "commit" | "end" | "rollback" | "abort" | "savepoint" | "release", ..] => true,
        ["prepare", "transaction", ..] | ["vacuum", ..] | ["cluster"] | ["alter", "system", ..] | ["discard", "all"] => true,
        ["create" | "drop", "database" | "tablespace", ..] => true,
        ["create", "index", "concurrently", ..] | ["create", "unique", "index", "concurrently", ..] => true,
        ["drop", "index", "concurrently", ..] => true,
        ["reindex", rest @ ..] => rest.contains(&"concurrently") || matches!(rest.first(), Some(&"database" | &"system")),
        ["create" | "alter" | "drop", "subscription", ..] => true,
        ["set", "cluster", "setting", ..] | ["backup" | "restore" | "import", ..] if v == Variant::Cockroach => true,
        ["create" | "drop", "external", ..] if v == Variant::Redshift => true,
        _ => false,
    }
}

/// `COMMIT` / `END`: in an aborted transaction the server rolls it back.
pub(crate) fn is_commit(head: &[String]) -> bool {
    matches!(head.first().map(String::as_str), Some("commit" | "end")) && head.get(1).map(String::as_str) != Some("prepared")
}

/// ROLLBACK [WORK | TRANSACTION] TO [SAVEPOINT] name.
fn rollback_to(head: &[String]) -> bool {
    head.first().map(String::as_str) == Some("rollback") && head.iter().skip(1).take(2).any(|w| w == "to")
}

/// Transaction control: these change the state in ways a multi-statement
/// text hides, so the state is asked again.
pub(crate) fn controls_transaction(head: &[String]) -> bool {
    matches!(
        head.first().map(String::as_str),
        Some("begin" | "start" | "commit" | "end" | "rollback" | "abort" | "prepare" | "savepoint" | "release")
    )
}

/// The session's transaction after a statement ran (`ok`) or failed.
pub(crate) fn next_state(v: Variant, prev: TxState, head: &[String], ok: bool) -> TxState {
    let first = head.first().map(String::as_str).unwrap_or("");
    if rollback_to(head) {
        return if ok && prev == TxState::Failed { TxState::Open } else if ok { prev } else { failed(v, prev) };
    }
    match first {
        "begin" | "start" if ok => TxState::Open,
        // A failed COMMIT (a deferred constraint, a serialization failure)
        // still ends the transaction.
        "commit" | "end" | "rollback" | "abort" if head.get(1).map(String::as_str) != Some("prepared") => TxState::Idle,
        "prepare" if ok && head.get(1).map(String::as_str) == Some("transaction") => TxState::Idle,
        _ if ok => prev,
        _ => failed(v, prev),
    }
}

fn failed(v: Variant, prev: TxState) -> TxState {
    if prev == TxState::Open && v.aborts_on_error() {
        TxState::Failed
    } else {
        prev
    }
}

/// A statement's failure with what the server says about it: SQLSTATE,
/// position (as an offset and line of `text`), PL/pgSQL context. FATAL
/// errors end the script (the server closes the session).
pub(crate) fn statement_error(e: tokio_postgres::Error, text: &str) -> Error {
    let Some(db) = e.as_db_error() else { return err(e) };
    if db.code() == &SqlState::QUERY_CANCELED {
        return Error::Cancelled;
    }
    let mut message = db_text(db);
    if let Some(w) = db.where_() {
        message.push_str(&format!("\nContexto: {w}"));
    }
    if db.code() == &SqlState::IN_FAILED_SQL_TRANSACTION {
        message.push_str("\nLa transacción tiene un error: deshacela (ROLLBACK) para seguir.");
    }
    let code = db.code().code();
    let mut se = ScriptError::new(message).with_code(code).with_sqlstate(code);
    if let Some(ErrorPosition::Original(p)) = db.position() {
        let at = char_offset(text, *p as usize);
        se = se.at_offset(at).at_line(line_of(text, at));
    }
    if matches!(db.parsed_severity(), Some(Severity::Fatal | Severity::Panic)) || e.is_closed() {
        se = se.fatal();
    }
    Error::Statement(Box::new(se))
}

/// A notice (RAISE NOTICE, WARNING, "table does not exist, skipping"…) as
/// a message of the run; WARNING is a warning, the rest information.
pub(crate) fn notice(out: &mut QueryOutcome, n: &DbError) {
    let warning = match n.parsed_severity() {
        Some(s) => s == Severity::Warning,
        None => n.severity() == "WARNING",
    };
    let code = n.code().code();
    out.message(Message {
        level: if warning { MessageLevel::Warning } else { MessageLevel::Info },
        text: db_text(n),
        code: (code != "00000").then(|| code.to_string()),
        ..Default::default()
    });
}

/// Byte offset of the server's 1-based character position.
fn char_offset(text: &str, position: usize) -> usize {
    text.char_indices().nth(position.saturating_sub(1)).map_or(text.len(), |(i, _)| i)
}

fn line_of(text: &str, offset: usize) -> u32 {
    text.as_bytes()[..offset.min(text.len())].iter().filter(|&&c| c == b'\n').count() as u32 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<String> {
        head(s)
    }

    fn tag_of(s: &str, n: u64) -> Option<String> {
        let hd = head(s);
        tag(&hd, &verb(s, &hd), n)
    }

    #[test]
    fn tags_follow_psql() {
        assert_eq!(tag_of("insert into t values (1)", 1).as_deref(), Some("INSERT 0 1"));
        assert_eq!(tag_of("/* x */ UPDATE t SET a = 1", 4).as_deref(), Some("UPDATE 4"));
        assert_eq!(tag_of("delete from t", 0).as_deref(), Some("DELETE 0"));
        assert_eq!(tag_of("select * from t", 7).as_deref(), Some("SELECT 7"));
        assert_eq!(tag_of("values (1), (2)", 2).as_deref(), Some("SELECT 2"));
        assert_eq!(tag_of("copy t from '/tmp/x.csv'", 9).as_deref(), Some("COPY 9"));
        assert_eq!(tag_of("create table t (a int)", 0).as_deref(), Some("CREATE TABLE"));
        assert_eq!(tag_of("CREATE OR REPLACE FUNCTION f() RETURNS int AS $$ select 1 $$ LANGUAGE sql", 0).as_deref(), Some("CREATE FUNCTION"));
        assert_eq!(tag_of("create unique index concurrently i on t (a)", 0).as_deref(), Some("CREATE INDEX"));
        assert_eq!(tag_of("create materialized view m as select 1", 1).as_deref(), Some("CREATE MATERIALIZED VIEW"));
        assert_eq!(tag_of("create temp table x (a int)", 0).as_deref(), Some("CREATE TABLE"));
        assert_eq!(tag_of("drop table if exists t", 0).as_deref(), Some("DROP TABLE"));
        assert_eq!(tag_of("create foreign data wrapper w", 0).as_deref(), Some("CREATE FOREIGN DATA WRAPPER"));
        assert_eq!(tag_of("alter default privileges grant select on tables to r", 0).as_deref(), Some("ALTER DEFAULT PRIVILEGES"));
        assert_eq!(tag_of("truncate t", 0).as_deref(), Some("TRUNCATE TABLE"));
        assert_eq!(tag_of("begin", 0).as_deref(), Some("BEGIN"));
        assert_eq!(tag_of("end", 0).as_deref(), Some("COMMIT"));
        assert_eq!(tag_of("set search_path = x", 0).as_deref(), Some("SET"));
        assert_eq!(tag_of("do $$ begin end $$", 0).as_deref(), Some("DO"));
        assert_eq!(tag_of("vacuum", 0).as_deref(), Some("VACUUM"));
        assert_eq!(tag_of("execute p", 3), None);
        // WITH: the verb after the CTEs, past parentheses and quotes.
        assert_eq!(tag_of("with x as (select ')' as a) insert into t select * from x", 1).as_deref(), Some("INSERT 0 1"));
        assert_eq!(tag_of("with d as (delete from t returning *) select count(*) from d", 1).as_deref(), Some("SELECT 1"));
        assert_eq!(tag_of("with f as (select $q$ ) update $q$) delete from t", 2).as_deref(), Some("DELETE 2"));
    }

    #[test]
    fn counts_only_where_the_tag_has_them() {
        assert_eq!(affected("insert", 3), Some(3));
        assert_eq!(affected("delete", 0), Some(0));
        assert_eq!(affected("create", 0), None);
        // CREATE TABLE … AS: the server reports the rows written.
        assert_eq!(affected("create", 5), Some(5));
        assert_eq!(affected("set", 0), None);
    }

    #[test]
    fn manual_mode_begins_like_psql() {
        let nb = |s: &str| no_begin(Variant::Postgres, &h(s));
        assert!(!nb("insert into t values (1)"));
        assert!(!nb("select 1"));
        assert!(!nb("create index i on t (a)"));
        assert!(nb("BEGIN"));
        assert!(nb("start transaction isolation level serializable"));
        assert!(nb("commit"));
        assert!(nb("vacuum analyze t"));
        assert!(nb("create database d"));
        assert!(nb("drop database d"));
        assert!(nb("create index concurrently i on t (a)"));
        assert!(nb("create unique index concurrently i on t (a)"));
        assert!(nb("drop index concurrently i"));
        assert!(nb("reindex table concurrently t"));
        assert!(nb("reindex database d"));
        assert!(!nb("reindex table t"));
        assert!(nb("alter system set work_mem = '8MB'"));
        assert!(nb("-- just a comment"));
        assert!(no_begin(Variant::Cockroach, &h("set cluster setting x = 1")));
        assert!(!no_begin(Variant::Postgres, &h("set cluster setting x = 1")));
        assert!(no_begin(Variant::Redshift, &h("create external table s.t (a int)")));
    }

    #[test]
    fn transaction_state_follows_the_statements() {
        let pg = Variant::Postgres;
        let next = |prev, s: &str, ok| next_state(pg, prev, &h(s), ok);
        assert_eq!(next(TxState::Idle, "begin", true), TxState::Open);
        assert_eq!(next(TxState::Idle, "insert into t values (1)", false), TxState::Idle);
        assert_eq!(next(TxState::Open, "insert into t values (1)", true), TxState::Open);
        assert_eq!(next(TxState::Open, "insert into t values (1)", false), TxState::Failed);
        assert_eq!(next(TxState::Failed, "select 1", false), TxState::Failed);
        assert_eq!(next(TxState::Failed, "rollback to savepoint a", true), TxState::Open);
        assert_eq!(next(TxState::Failed, "rollback work to a", true), TxState::Open);
        assert_eq!(next(TxState::Open, "rollback to a", false), TxState::Failed);
        assert_eq!(next(TxState::Failed, "commit", true), TxState::Idle);
        assert_eq!(next(TxState::Open, "commit", false), TxState::Idle);
        assert_eq!(next(TxState::Open, "end", true), TxState::Idle);
        assert_eq!(next(TxState::Open, "abort", true), TxState::Idle);
        assert_eq!(next(TxState::Open, "prepare transaction 'x'", true), TxState::Idle);
        assert_eq!(next(TxState::Idle, "commit prepared 'x'", true), TxState::Idle);
        // H2 doesn't abort the transaction on an error.
        assert_eq!(next_state(Variant::H2, TxState::Open, &h("insert into t values (1)"), false), TxState::Open);
        assert!(is_commit(&h("COMMIT")) && is_commit(&h("end transaction")) && !is_commit(&h("commit prepared 'x'")));
    }

    #[test]
    fn positions_are_byte_offsets_and_lines() {
        let text = "select 1;\nselect ñandú frm t";
        // The server counts characters from 1.
        let char_pos = text[..text.find("frm").unwrap()].chars().count() + 1;
        let at = char_offset(text, char_pos);
        assert_eq!(&text[at..at + 3], "frm");
        assert_eq!(line_of(text, at), 2);
        assert_eq!(char_offset(text, 0), 0);
        assert_eq!(char_offset(text, 999), text.len());
    }

    #[test]
    fn the_lexer_keeps_bodies_whole() {
        let texts = |s: &str| sql::split_script(s, &DIALECT).into_iter().map(|u| u.text).collect::<Vec<_>>();
        let f = "create function f() returns int as $body$ begin; return 1; end $body$ language plpgsql;\nselect f()";
        assert_eq!(texts(f).len(), 2);
        assert_eq!(texts(r"select E'a\'; b'; select 2").len(), 2);
        assert_eq!(texts("select /* a /* nested; */ still */ 1; select 2").len(), 2);
        let atomic = "create function g() returns int language sql begin atomic select 1; select 2; end;\nselect g()";
        assert_eq!(texts(atomic).len(), 2, "{:?}", texts(atomic));
        assert_eq!(texts("do $$ begin raise notice 'a;b'; end $$; vacuum").len(), 2);
    }

    #[test]
    fn variants() {
        assert!(Variant::Postgres.manual_transactions() && Variant::Redshift.manual_transactions());
        assert!(!Variant::Materialize.manual_transactions() && !Variant::CrateDb.manual_transactions());
        assert!(Variant::Cockroach.probes_transaction() && !Variant::Redshift.probes_transaction());
        assert!(Variant::Postgres.continue_on_error() && !Variant::Cockroach.continue_on_error());
    }
}
