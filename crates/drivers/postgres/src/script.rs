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
use dbine_driver::sql::{self, ScriptDialect, ScriptStatement, StatementKind};
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

/// What psql does itself, found at the start of a statement's text.
#[derive(Debug, PartialEq)]
pub(crate) enum ClientCommand {
    /// `\echo`, `\qecho`, `\warn`: their text.
    Echo(String),
    /// `\connect` / `\c`: another database, which this session can't reach.
    Connect(String),
    /// Another meta-command (`\set`, `\pset`…), as written.
    Ignored(String),
    /// `\restrict` / `\unrestrict` of pg_dump: nothing to do outside psql.
    Silent,
    /// The rows of a `COPY … FROM stdin`, up to their `\.`.
    CopyData,
}

/// psql meta-commands (`\echo`, `\connect`, pg_dump's `\restrict`…) and
/// `COPY … FROM stdin` data ending in `\.`: psql handles them, the server
/// would see a syntax error. The lexer leaves them in front of the next
/// statement; here they're blanked out byte for byte (the server's
/// positions and lines stay right) and returned.
pub(crate) fn client_commands(text: &str) -> (String, Vec<ClientCommand>) {
    let mut found = Vec::new();
    let mut bytes = text.as_bytes().to_vec();
    let blank = |from: usize, to: usize, bytes: &mut Vec<u8>| {
        for b in &mut bytes[from..to] {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
    };
    // Lines with their byte ranges.
    let mut lines = Vec::new();
    let mut at = 0;
    for l in text.split_inclusive('\n') {
        lines.push((at, at + l.len(), l.trim_end_matches(['\n', '\r'])));
        at += l.len();
    }
    // COPY data: everything up to its `\.` line, when that's all there is
    // before it (a text of several statements keeps its SQL).
    let mut first = 0;
    let data_end = lines.iter().position(|(_, _, l)| l.trim() == r"\.");
    // A unit that opens with an SQL statement isn't COPY data, even with a
    // `\.` line inside it (a string literal can hold one).
    let starts_as_sql = lines.iter().map(|(_, _, l)| l.trim()).find(|l| !l.is_empty() && !l.starts_with("--")).is_some_and(|l| {
        let word: String = l.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
        matches!(
            word.as_str(),
            "select" | "with" | "insert" | "update" | "delete" | "merge" | "create" | "alter" | "drop" | "copy" | "do" | "begin"
                | "start" | "commit" | "rollback" | "set" | "reset" | "values" | "table" | "explain" | "analyze" | "vacuum" | "grant"
                | "revoke" | "truncate" | "call" | "comment" | "show" | "prepare" | "execute" | "declare" | "fetch" | "listen"
                | "notify" | "lock" | "refresh" | "reindex" | "cluster" | "security" | "import"
        )
    });
    if let Some(end) = data_end.filter(|&e| !starts_as_sql && !text[..lines[e].0].contains(';')) {
        blank(0, lines[end].1, &mut bytes);
        found.push(ClientCommand::CopyData);
        first = end + 1;
    }
    for &(from, to, line) in &lines[first..] {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("--") {
            continue;
        }
        let Some(cmd) = t.strip_prefix('\\') else { break };
        let (name, arg) = cmd.split_once(char::is_whitespace).map_or((cmd, ""), |(n, a)| (n, a.trim()));
        found.push(match name {
            "echo" | "qecho" | "warn" => {
                let arg = arg.strip_prefix("-n ").unwrap_or(arg);
                let unquoted = arg.strip_prefix('\'').and_then(|a| a.strip_suffix('\'')).map(|a| a.replace("''", "'"));
                ClientCommand::Echo(unquoted.unwrap_or_else(|| arg.to_string()))
            }
            "c" | "connect" => ClientCommand::Connect(arg.to_string()),
            "restrict" | "unrestrict" => ClientCommand::Silent,
            _ => ClientCommand::Ignored(t.to_string()),
        });
        blank(from, to, &mut bytes);
    }
    // Only ASCII bytes were replaced by ASCII spaces, whole lines at a time.
    (String::from_utf8(bytes).unwrap_or_else(|_| text.to_string()), found)
}

/// The script cut into the units psql would run. The lexer reads SQL; on
/// top of it, as psql: a line starting with `\` (outside quotes, comments
/// and bodies) is a meta-command that ends at its newline, and the lines
/// after `COPY … FROM stdin;` are data up to their `\.` line. Both would
/// otherwise reach the lexer as SQL, where an apostrophe in them (`\echo
/// it's`, a row with `O'Brien`) opens a string that swallows the
/// statements after it.
///
/// Meta-commands are units `execute` handles ([`client_commands`]), except
/// pg_dump's `\restrict` / `\unrestrict`, which need nothing. COPY data is
/// a client unit: the app doesn't send it, and its COPY already says why.
pub(crate) fn split(text: &str) -> Vec<ScriptStatement> {
    /// Ends the text the lexer sees: whether it comes back as a unit of its
    /// own tells if the text ended outside quotes, comments and bodies.
    const PROBE: &str = "\n;dbine_probe";
    let mut out = Vec::new();
    let mut pos = 0;
    let mut line = 1;
    let push = |out: &mut Vec<ScriptStatement>, mut u: ScriptStatement, base: usize, base_line: u32| {
        u.start += base;
        u.end += base;
        u.line += base_line - 1;
        out.push(u);
    };
    while pos < text.len() {
        // The next line that starts with `\`: a meta-command, the `\.` of
        // COPY data, or text inside a quote (then it's taken in).
        let mut cut = backslash_line(text, pos, pos);
        let (units, cut) = loop {
            let mut seg = text[pos..cut].to_string();
            if cut == text.len() {
                break (sql::split_script(&seg, &DIALECT), cut);
            }
            seg.push_str(PROBE);
            let mut units = sql::split_script(&seg, &DIALECT);
            let clean = units.last().is_some_and(|u| u.text == PROBE[2..] && u.start == cut - pos + 2);
            let copy = units.iter().any(|u| copy_from_stdin(&u.text, &head(&u.text)));
            if clean || copy {
                if clean {
                    units.pop();
                }
                break (units, cut);
            }
            cut = backslash_line(text, pos, cut + 1);
        };
        let mut next = cut;
        for u in units {
            let is_copy = copy_from_stdin(&u.text, &head(&u.text));
            let end = u.end + pos;
            push(&mut out, u, pos, line);
            if is_copy {
                // Data: from the line after the terminator to its `\.` line.
                let after = &text[end..];
                let term = end + (after.len() - after.trim_start().len()) + usize::from(after.trim_start().starts_with(';'));
                let start = text[term..].find('\n').map_or(text.len(), |n| term + n + 1);
                let mut stop = text.len();
                let mut at = start;
                for l in text[start..].split_inclusive('\n') {
                    at += l.len();
                    if l.trim() == r"\." {
                        stop = at;
                        break;
                    }
                }
                let data = text[start..stop].trim_end();
                if !data.is_empty() {
                    out.push(ScriptStatement {
                        text: data.to_string(),
                        start,
                        end: start + data.len(),
                        line: line + text[pos..start].bytes().filter(|&b| b == b'\n').count() as u32,
                        kind: StatementKind::ClientCommand,
                        repeat: 1,
                        error: None,
                    });
                }
                next = stop;
                break;
            }
        }
        if next == cut && cut < text.len() {
            // The meta-command line at `cut`.
            let eol = text[cut..].find('\n').map_or(text.len(), |n| cut + n);
            let raw = &text[cut..eol];
            let cmd = raw.trim();
            let start = cut + (raw.len() - raw.trim_start().len());
            let name = cmd[1..].split(char::is_whitespace).next().unwrap_or("");
            let silent = matches!(name, "restrict" | "unrestrict");
            out.push(ScriptStatement {
                text: cmd.to_string(),
                start,
                end: start + cmd.len(),
                line: line + text[pos..start].bytes().filter(|&b| b == b'\n').count() as u32,
                kind: if silent { StatementKind::ClientCommand } else { StatementKind::Sql },
                repeat: 1,
                error: None,
            });
            next = (eol + 1).min(text.len());
        }
        line += text[pos..next].bytes().filter(|&b| b == b'\n').count() as u32;
        pos = next;
    }
    out
}

/// Where the first line starting with `\` (after spaces) begins, among the
/// lines that start at or after `from` (`pos` being a line start), or the
/// end of `text`.
fn backslash_line(text: &str, pos: usize, from: usize) -> usize {
    let mut at = pos;
    for l in text[pos..].split_inclusive('\n') {
        if at >= from && l.trim_start().starts_with('\\') {
            return at;
        }
        at += l.len();
    }
    text.len()
}

/// `USE db` / `SET [SESSION] database = db` (CockroachDB, Materialize):
/// the session now works on another database.
pub(crate) fn switches_database(head: &[String]) -> bool {
    let w: Vec<&str> = head.iter().map(String::as_str).collect();
    matches!(w.as_slice(), ["use", _, ..] | ["set", "database", ..] | ["set", "session", "database", ..])
}

/// Statements whose column types are asked inside an open transaction,
/// after they ran: queries the server just parsed and planned, so the
/// describe can't fail where they didn't (and abort the transaction).
pub(crate) fn describable_in_transaction(verb: &str) -> bool {
    matches!(verb, "select" | "values" | "table" | "insert" | "update" | "delete" | "merge")
}

/// `COPY … FROM STDIN`: the server would wait for rows the editor doesn't
/// send, and the client library can't answer it in a simple query.
pub(crate) fn copy_from_stdin(text: &str, head: &[String]) -> bool {
    if head.first().map(String::as_str) != Some("copy") {
        return false;
    }
    let t = sql::strip_comments(text, &DIALECT, false).to_ascii_lowercase();
    let words: Vec<&str> = t.split_whitespace().collect();
    words.windows(2).any(|w| w[0] == "from" && w[1].trim_end_matches(';') == "stdin")
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
                    let close = t[e + 1..].find(tag)?;
                    i = e + 1 + close + tag.len() - 1;
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
    fn psql_meta_commands_are_the_clients() {
        let text = "\\restrict abc\n-- note\n\\echo 'hola ''mundo'''\n\\set ON_ERROR_STOP on\nselect ñ frm t";
        let (clean, found) = client_commands(text);
        assert_eq!(clean.len(), text.len());
        assert_eq!(clean.lines().count(), text.lines().count());
        assert!(clean.ends_with("select ñ frm t") && !clean.contains('\\'), "{clean:?}");
        assert_eq!(
            found,
            [ClientCommand::Silent, ClientCommand::Echo("hola 'mundo'".into()), ClientCommand::Ignored("\\set ON_ERROR_STOP on".into())]
        );
        let (clean, found) = client_commands("1\ta\n2\tb\n\\.\n\n\\connect other\nselect 1");
        assert_eq!(found, [ClientCommand::CopyData, ClientCommand::Connect("other".into())]);
        assert_eq!(clean.trim(), "select 1");
        // Statements before the data (a whole file's chunk): kept.
        let chunk = "copy t from stdin;\n1\ta\n\\.\nselect 1";
        assert_eq!(client_commands(chunk), (chunk.to_string(), vec![]));
        // A string literal holding a `\.` line is SQL, not COPY data.
        let literal = "SELECT 'a\n\\.\nb' AS s";
        assert_eq!(client_commands(literal), (literal.to_string(), vec![]));
        // Not at the start: SQL, left alone.
        let (clean, found) = client_commands("select '\\echo'");
        assert!(found.is_empty() && clean == "select '\\echo'");
        assert!(copy_from_stdin("COPY public.t (a, b) FROM stdin", &h("COPY public.t (a, b) FROM stdin")));
        assert!(copy_from_stdin("copy t from STDIN with (format csv);", &h("copy t")));
        assert!(!copy_from_stdin("copy t to stdout", &h("copy t to stdout")));
        assert!(!copy_from_stdin("select 'from stdin'", &h("select")));
    }

    fn units(s: &str) -> Vec<(String, u32, StatementKind)> {
        let st = split(s);
        for u in &st {
            assert_eq!(&s[u.start..u.end], u.text, "{u:?}");
            assert_eq!(line_of(s, u.start), u.line, "{u:?}");
        }
        st.into_iter().map(|u| (u.text, u.line, u.kind)).collect()
    }

    #[test]
    fn copy_data_and_meta_commands_end_where_psql_ends_them() {
        use StatementKind::{ClientCommand as C, Sql as S};
        let s = "\\restrict X\nSET a = 1;\nCOPY public.v2_t (id, n) FROM stdin;\n1\tO'Brien\n\\N\ta;b\n\\.\n\nSELECT 'after copy' AS x;\n\\unrestrict X";
        assert_eq!(
            units(s),
            [
                ("\\restrict X".into(), 1, C),
                ("SET a = 1".into(), 2, S),
                ("COPY public.v2_t (id, n) FROM stdin".into(), 3, S),
                ("1\tO'Brien\n\\N\ta;b\n\\.".into(), 4, C),
                ("SELECT 'after copy' AS x".into(), 8, S),
                ("\\unrestrict X".into(), 9, C),
            ]
        );
        assert_eq!(
            units("\\echo it's done\nSELECT 1 AS a;\nSELECT 2 AS b;\n  \\set x 1;\nselect 3"),
            [
                ("\\echo it's done".into(), 1, S),
                ("SELECT 1 AS a".into(), 2, S),
                ("SELECT 2 AS b".into(), 3, S),
                ("\\set x 1;".into(), 4, S),
                ("select 3".into(), 5, S),
            ]
        );
        // A `\` line inside a string, a body or a comment is theirs.
        let s = "select 'a\n\\b';\ndo $$\n\\echo x\n$$;\n/*\n\\c y\n*/ select 2;";
        assert_eq!(units(s).iter().map(|u| u.0.as_str()).collect::<Vec<_>>(), ["select 'a\n\\b'", "do $$\n\\echo x\n$$", "select 2"]);
        // Data with no `\.` runs to the end; two COPYs in a row.
        let s = "copy a from stdin;\n1\tx'\n\\.\ncopy b from stdin;\n2\t'y;\n";
        assert_eq!(
            units(s),
            [
                ("copy a from stdin".into(), 1, S),
                ("1\tx'\n\\.".into(), 2, C),
                ("copy b from stdin".into(), 4, S),
                ("2\t'y;".into(), 5, C),
            ]
        );
        // Plain SQL: the lexer's units as they are.
        let s = "select 1;\n-- c\nselect $$a;b$$;";
        assert_eq!(split(s), sql::split_script(s, &DIALECT));
    }

    #[test]
    fn variants() {
        assert!(Variant::Postgres.manual_transactions() && Variant::Redshift.manual_transactions());
        assert!(!Variant::Materialize.manual_transactions() && !Variant::CrateDb.manual_transactions());
        assert!(Variant::Cockroach.probes_transaction() && !Variant::Redshift.probes_transaction());
        assert!(Variant::Postgres.continue_on_error() && !Variant::Cockroach.continue_on_error());
    }
}
