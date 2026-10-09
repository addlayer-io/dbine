//! Script splitting: one configurable lexer for every engine.
//!
//! [`split_script`] cuts a script into the units the app sends one at a
//! time ([`ScriptStatement`]), each with its byte range and line in the
//! original text. A [`ScriptDialect`] says what the engine's own tool
//! understands: its quotes and comments (so a `;` or a `GO` inside them
//! never splits), block syntax that holds `;` inside (PL/SQL, trigger
//! bodies), batch separator lines (`GO [N]`, `/`) and terminator switches
//! (`DELIMITER`, `SET TERM`, `--#SET TERMINATOR`).
//!
//! The lexer never fails: an unclosed quote or comment runs to the end of
//! the script, and the server reports it.

use serde::{Deserialize, Serialize};

/// What a [`ScriptStatement`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    /// A statement ended by the terminator (`;`).
    Sql,
    /// A block that holds `;` inside: a PL/SQL unit ended by a `/` line, a
    /// trigger or routine body (`BEGIN … END`).
    Block,
    /// A T-SQL batch: everything up to a `GO` line, sent whole.
    Batch,
    /// A command for the client, not the server (`DELIMITER //`,
    /// `SET TERM ^ ;`). The lexer already applied it: it is listed so the
    /// UI can show it, and the app doesn't send it.
    ClientCommand,
}

/// One unit of a script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptStatement {
    /// `script[start..end]`: from its first token (leading comments and
    /// blank lines left out) to its last, without the terminator.
    pub text: String,
    /// Byte offsets in the original script.
    pub start: usize,
    pub end: usize,
    /// 1-based line of `start`.
    pub line: u32,
    pub kind: StatementKind,
    /// Times to run it: `GO 5` runs its batch 5 times. 1 otherwise.
    #[serde(default = "one")]
    pub repeat: u32,
    /// A client-side error the lexer found here (a `GO` line with an
    /// invalid count): the app reports it and runs nothing of the script,
    /// as SSMS does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The message of a `GO` line whose count isn't a valid number of runs.
pub const GO_COUNT_ERROR: &str = "GO: número de repeticiones inválido";

fn one() -> u32 {
    1
}

/// Separator lines: a line holding only the separator ends the statement
/// or batch, wherever the terminator is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchLine {
    /// `GO`, `GO 5`, `GO -- comment` (SQL Server, Sybase): case-insensitive,
    /// alone on its line, never inside a comment or string. `GO;` isn't one.
    Go,
    /// `/` alone on its line (SQL*Plus): ends a PL/SQL block or a statement.
    Slash,
    /// No separator lines. Also what a value this build doesn't know reads
    /// as (a newer host).
    #[default]
    #[serde(other)]
    None,
}

/// What an engine's own tool understands when it splits a script. Start
/// from a preset ([`ScriptDialect::generic`], [`ScriptDialect::postgres`]…)
/// and change what differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptDialect {
    /// The terminator (`;`, or the one switched to) ends a statement.
    /// `false`: only batch lines split (T-SQL: a batch goes whole).
    pub semicolons: bool,
    /// `$$ … $$` and `$tag$ … $tag$` (PostgreSQL, DuckDB). Also turns on
    /// backslash escapes in `E'…'` strings.
    pub dollar_quotes: bool,
    /// `\'` escapes a quote inside '…' and "…" (MySQL, ClickHouse).
    pub backslash_escapes: bool,
    /// `"…"` is an identifier that only escapes by doubling `""`, even when
    /// '…' takes backslash escapes (Snowflake).
    pub dquote_idents: bool,
    /// `/* /* */ */` nests (PostgreSQL, SQL Server).
    pub nested_comments: bool,
    /// `#` starts a line comment (MySQL).
    pub hash_comments: bool,
    /// Oracle's `q'[ … ]'` (any delimiter; brackets close with their pair).
    pub q_quotes: bool,
    /// `[name]` is an identifier (SQL Server, Sybase, Access).
    pub bracket_idents: bool,
    /// `` `name` `` is an identifier (MySQL, BigQuery, ClickHouse, Spark…).
    pub backtick_idents: bool,
    pub batch: BatchLine,
    /// `DELIMITER x` on its own line switches the terminator (mysql CLI).
    pub delimiter_command: bool,
    /// `SET TERM x ;` switches the terminator (Firebird isql).
    pub set_term: bool,
    /// `--#SET TERMINATOR x` switches the terminator (DB2 CLP).
    pub terminator_directive: bool,
    /// PL/SQL units (`CREATE [OR REPLACE] PROCEDURE|FUNCTION|PACKAGE|
    /// TRIGGER|TYPE…`, `DECLARE`, `BEGIN`) hold `;` and end at a `/` line
    /// (or the end of the script), as in SQL*Plus.
    pub plsql_blocks: bool,
    /// A `CREATE`/`ALTER` of a TRIGGER, PROCEDURE, FUNCTION or EVENT whose
    /// body is `BEGIN … END` holds `;` until its last `END` (SQLite
    /// triggers, SQL-standard bodies, MySQL routines written without
    /// `DELIMITER`).
    pub compound_blocks: bool,
    /// T-SQL: a `CREATE`/`ALTER` of a PROCEDURE, FUNCTION, TRIGGER or VIEW
    /// runs to the end of its batch (its body may hold `;` and needs no
    /// `BEGIN`), and a `BEGIN … END` keeps its `;` inside. Only matters
    /// when splitting statement by statement ([`Self::statements`]).
    pub tsql_blocks: bool,
    /// `--` starts a comment only when a space, a control character or the
    /// end follows it (MySQL: `a--1` is `a - -1`).
    pub dash_comment_space: bool,
}

impl Default for ScriptDialect {
    fn default() -> Self {
        Self::generic()
    }
}

impl ScriptDialect {
    /// `;` outside '…', "…", `…`, `--` and `/* */`; trigger and routine
    /// bodies kept whole. What the contract assumes when a driver says
    /// nothing.
    pub const fn generic() -> Self {
        Self {
            semicolons: true,
            dollar_quotes: false,
            backslash_escapes: false,
            dquote_idents: false,
            nested_comments: false,
            hash_comments: false,
            q_quotes: false,
            bracket_idents: false,
            backtick_idents: true,
            batch: BatchLine::None,
            delimiter_command: false,
            set_term: false,
            terminator_directive: false,
            plsql_blocks: false,
            compound_blocks: true,
            tsql_blocks: false,
            dash_comment_space: false,
        }
    }

    /// The preset for an editor dialect hint ([`crate::DriverInfo::dialect`]):
    /// what [`crate::Driver::script_dialect`] returns when the driver says
    /// nothing. Unknown hints get [`Self::generic`].
    pub fn for_hint(hint: &str) -> Self {
        match hint {
            "postgres" => Self::postgres(),
            "mysql" => Self::mysql(),
            "mssql" | "sybase" => Self::tsql(),
            "oracle" => Self::oracle(),
            "db2" => Self::db2(),
            // `name` in backticks, '…' and "…" strings with backslash escapes.
            "tdengine" => Self { backslash_escapes: true, ..Self::generic() },
            _ => Self::generic(),
        }
    }

    /// psql: dollar quotes, `E'…'` escapes, nested comments.
    pub const fn postgres() -> Self {
        Self { dollar_quotes: true, nested_comments: true, backtick_idents: false, ..Self::generic() }
    }

    /// mysql CLI: backslash escapes, `#` comments, `-- ` comments only with
    /// the space, `DELIMITER`.
    pub const fn mysql() -> Self {
        Self { backslash_escapes: true, hash_comments: true, delimiter_command: true, dash_comment_space: true, ..Self::generic() }
    }

    /// SSMS / sqlcmd: batches separated by `GO [N]` lines, `[ident]`,
    /// nested comments; routine bodies and `BEGIN … END` kept whole.
    pub const fn tsql() -> Self {
        Self {
            semicolons: false,
            nested_comments: true,
            bracket_idents: true,
            backtick_idents: false,
            batch: BatchLine::Go,
            compound_blocks: false,
            tsql_blocks: true,
            ..Self::generic()
        }
    }

    /// SQL*Plus / SQL Developer: `q'[…]'`, PL/SQL blocks ended by `/`.
    pub const fn oracle() -> Self {
        Self {
            q_quotes: true,
            backtick_idents: false,
            batch: BatchLine::Slash,
            plsql_blocks: true,
            compound_blocks: false,
            ..Self::generic()
        }
    }

    /// Firebird isql: `SET TERM`.
    pub const fn firebird() -> Self {
        Self { set_term: true, backtick_idents: false, ..Self::generic() }
    }

    /// DB2 CLP: `--#SET TERMINATOR`.
    pub const fn db2() -> Self {
        Self { terminator_directive: true, backtick_idents: false, ..Self::generic() }
    }

    /// The same dialect, also splitting on the terminator inside batches:
    /// statement by statement, for "run the statement at the cursor", the
    /// read-only guard and the UPDATE/DELETE check.
    pub const fn statements(self) -> Self {
        Self { semicolons: true, ..self }
    }
}

/// How the app runs an editor script on a driver (see
/// [`crate::Driver::script_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptMode {
    /// The app splits the script ([`crate::Driver::split_script`]) and calls
    /// `execute` once per statement, reporting each one as it ends.
    PerStatement,
    /// Like `PerStatement`, but the units are the dialect's batches (T-SQL
    /// `GO`), each run `repeat` times (`GO 5`).
    Batches,
    /// `execute` gets the whole script in one call: engines that must get
    /// it in one request (Snowflake, BigQuery scripting, InfluxDB v1), and
    /// every driver not yet adapted (the driver splits and stops at the
    /// first error, as before). Also what a mode this build doesn't know
    /// reads as (a newer host).
    #[default]
    #[serde(other)]
    Whole,
}

/// What the engine's own tool does by default with a script (see
/// [`crate::Driver::script_defaults`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptDefaults {
    /// Go on after a failed statement (SSMS, psql, SQL*Plus do; the mysql
    /// CLI and most others stop). The tab's toggle overrides it.
    #[serde(default)]
    pub continue_on_error: bool,
    /// Ask before running an `UPDATE` / `DELETE` without `WHERE`
    /// ([`unsafe_statements`]). SQL engines only.
    #[serde(default)]
    pub confirm_unsafe_dml: bool,
}

impl ScriptDefaults {
    /// Stop on errors; ask about unsafe DML on SQL engines.
    pub fn for_language(language: crate::Language) -> Self {
        Self { continue_on_error: false, confirm_unsafe_dml: language == crate::Language::Sql }
    }
}

/// Split `script` as `dialect`'s tool would. Empty statements (only
/// whitespace and comments) are dropped.
pub fn split_script(script: &str, dialect: &ScriptDialect) -> Vec<ScriptStatement> {
    Splitter::new(script, *dialect).run()
}

/// `text` without its comments (line comments leave their newline, block
/// comments a space), as drivers that send statements to an HTTP API want
/// them. `keep_hints`: optimizer hints (`/*+ … */`) and MySQL's versioned
/// comments (`/*! … */`) stay, since they change what runs.
pub fn strip_comments(text: &str, dialect: &ScriptDialect, keep_hints: bool) -> String {
    let sc = Scanner { s: text, b: text.as_bytes(), d: *dialect };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let (tok, end) = sc.token(i);
        match tok {
            Tok::LineComment => out.push('\n'),
            Tok::BlockComment => {
                if keep_hints && (text[i..].starts_with("/*+") || text[i..].starts_with("/*!")) {
                    out.push_str(&text[i..end]);
                } else {
                    out.push(' ');
                }
            }
            _ => out.push_str(&text[i..end]),
        }
        // A line comment ends before its newline: don't add it twice.
        i = if tok == Tok::LineComment && text.as_bytes().get(end) == Some(&b'\n') { end + 1 } else { end };
    }
    out
}

/// The first keyword of a statement, lowercase (`with`, `select`,
/// `update`…), skipping comments. `None` when it starts with something else.
pub fn leading_keyword(text: &str, dialect: &ScriptDialect) -> Option<String> {
    words(text, dialect).into_iter().next().map(|w| w.text.to_ascii_lowercase())
}

/// An `UPDATE` or `DELETE` that would touch every row of its table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsafeStatement {
    /// "UPDATE" or "DELETE".
    pub keyword: String,
    /// Byte offsets of the statement in the script, and its line.
    pub start: usize,
    pub end: usize,
    pub line: u32,
}

/// Every `UPDATE` / `DELETE` of `script` without a `WHERE` at its own
/// level (a `WHERE` inside a subquery doesn't count, a `LIMIT` doesn't
/// either), also those of a data-modifying CTE and those that follow
/// another statement without a `;` (T-SQL). Routine, trigger and PL/SQL
/// bodies aren't looked into: their DML runs when they're called, not
/// now. MySQL's versioned comments (`/*!50000 … */`) are read as code.
pub fn unsafe_statements(script: &str, dialect: &ScriptDialect) -> Vec<UnsafeStatement> {
    let script = expose_versioned(script, dialect);
    let mut out = Vec::new();
    for s in split_script(&script, &dialect.statements()) {
        if s.kind != StatementKind::Sql && s.kind != StatementKind::Batch {
            continue;
        }
        for hit in unsafe_hits(&s.text, dialect) {
            let start = s.start + hit.start;
            let line = s.line + s.text.as_bytes()[..hit.start].iter().filter(|&&c| c == b'\n').count() as u32;
            out.push(UnsafeStatement { keyword: hit.keyword.to_string(), start, end: s.start + hit.end, line });
        }
    }
    out
}

/// `Some("UPDATE" | "DELETE")` when `stmt` has one without a WHERE at its
/// own level (the first one; see [`unsafe_statements`]).
pub fn unsafe_dml(stmt: &str, dialect: &ScriptDialect) -> Option<&'static str> {
    unsafe_hits(stmt, dialect).first().map(|h| h.keyword)
}

/// `text` with MySQL / MariaDB versioned comments (`/*!50000 … */`,
/// `/*M!100101 … */`) opened up: their markers become spaces, so their
/// content reads as code (the server runs it) and offsets don't move.
pub fn expose_versioned<'a>(text: &'a str, dialect: &ScriptDialect) -> std::borrow::Cow<'a, str> {
    if !text.contains("/*!") && !text.contains("/*M!") {
        return std::borrow::Cow::Borrowed(text);
    }
    let sc = Scanner { s: text, b: text.as_bytes(), d: *dialect };
    let mut bytes = text.as_bytes().to_vec();
    let mut i = 0;
    while i < text.len() {
        let (tok, end) = sc.token(i);
        if tok == Tok::BlockComment {
            let open = if sc.starts(i, "/*!") { Some(3) } else if sc.starts(i, "/*M!") { Some(4) } else { None };
            if let Some(n) = open {
                let mut j = i + n;
                while j < end && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                bytes[i..j].fill(b' ');
                if end >= i + n + 2 && text[..end].ends_with("*/") {
                    bytes[end - 2..end].fill(b' ');
                }
            }
        }
        i = end.max(i + 1);
    }
    // Only ASCII bytes were replaced by spaces.
    std::borrow::Cow::Owned(String::from_utf8(bytes).unwrap_or_else(|_| text.to_string()))
}

/// An UPDATE / DELETE without WHERE inside a statement: byte range from
/// its first word (a `WITH` heading it included) to its last.
struct Hit {
    keyword: &'static str,
    start: usize,
    end: usize,
}

/// Words after which a DML keyword doesn't start a statement: `FOR UPDATE`,
/// `ON DELETE`, `ON DUPLICATE KEY UPDATE`, `THEN UPDATE` (MERGE),
/// `DO UPDATE` (ON CONFLICT), `GRANT UPDATE`, trigger events (`AFTER
/// INSERT OR UPDATE OF`, `INSTEAD OF DELETE`), `UNION SELECT`…
const NOT_A_START_AFTER: &[&str] = &[
    "for", "on", "key", "then", "do", "also", "instead", "of", "or", "before", "after", "grant", "revoke", "deny", "audit", "noaudit", "as",
    "union", "intersect", "except", "minus", "all", "distinct",
];

/// Keywords that start the next statement of a T-SQL batch written without
/// `;` (when they start one at all: see [`starts_statement`]).
const STATEMENT_KEYWORDS: &[&str] = &[
    "select", "insert", "update", "delete", "merge", "create", "alter", "drop", "truncate", "exec", "execute", "declare", "print", "grant",
    "revoke", "deny", "begin", "end", "commit", "rollback", "if", "else", "while", "return", "raiserror", "throw",
];

/// Whether `ws[k]` starts a statement rather than being part of one:
/// first, or after a word that doesn't take it (see [`NOT_A_START_AFTER`]),
/// a `)` or a `;`, or right inside a CTE's parentheses (`AS (DELETE …)`).
fn starts_statement(ws: &[Word<'_>], k: usize) -> bool {
    if k == 0 {
        return true;
    }
    let before = &ws[k - 1];
    match ws[k].prev {
        0 => !NOT_A_START_AFTER.iter().any(|x| before.is(x)),
        b')' | b';' => true,
        b'(' => before.is("as") || before.is("materialized"),
        _ => false,
    }
}

fn unsafe_hits(stmt: &str, dialect: &ScriptDialect) -> Vec<Hit> {
    let ws = words(stmt, dialect);
    let mut hits = Vec::new();
    for (k, w) in ws.iter().enumerate() {
        let keyword = if w.is("update") {
            // T-SQL's UPDATE STATISTICS isn't DML.
            if ws.get(k + 1).is_some_and(|n| n.is("statistics")) {
                continue;
            }
            "UPDATE"
        } else if w.is("delete") {
            "DELETE"
        } else {
            continue;
        };
        if !starts_statement(&ws, k) {
            continue;
        }
        // Its own WHERE: at its depth (outside CASE … END), before the
        // statement ends: its parenthesis closes or another one starts.
        let mut end = stmt.trim_end().len();
        let mut last = k;
        let mut case = 0u32;
        let mut has_where = false;
        for (j, x) in ws.iter().enumerate().skip(k + 1) {
            if x.depth < w.depth {
                end = ws[last].start + ws[last].text.len();
                break;
            }
            if x.depth == w.depth {
                if x.keyword("case") {
                    case += 1;
                } else if case > 0 {
                    if x.keyword("end") {
                        case -= 1;
                    }
                } else if x.is("where") || (x.is("use") && ws.get(j + 1).is_some_and(|n| n.is("keys"))) {
                    // SQL++ (Couchbase) `USE KEYS …` limits it to those
                    // documents, as a WHERE does.
                    has_where = true;
                    break;
                } else if STATEMENT_KEYWORDS.iter().any(|s| x.is(s)) && starts_statement(&ws, j) {
                    end = stmt[..x.start].trim_end_matches(|c: char| c.is_whitespace() || c == ';').len();
                    break;
                }
            }
            last = j;
        }
        if has_where {
            continue;
        }
        // A WITH heading it at its level is part of it.
        let mut start = w.start;
        for j in (0..k).rev() {
            let x = &ws[j];
            if x.depth < w.depth {
                break;
            }
            if x.depth == w.depth && starts_statement(&ws, j) {
                if x.is("with") {
                    start = x.start;
                    break;
                }
                if STATEMENT_KEYWORDS.iter().any(|s| x.is(s)) {
                    break;
                }
            }
        }
        hits.push(Hit { keyword, start, end: end.max(w.start + w.text.len()) });
    }
    hits
}

// --- Lexing -----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Space,
    LineComment,
    BlockComment,
    /// A string, quoted identifier, dollar or q-quote.
    Quoted,
    Word,
    /// Any other single character.
    Punct,
}

struct Scanner<'a> {
    s: &'a str,
    b: &'a [u8],
    d: ScriptDialect,
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

impl Scanner<'_> {
    fn at(&self, i: usize) -> u8 {
        self.b.get(i).copied().unwrap_or(0)
    }

    fn starts(&self, i: usize, p: &str) -> bool {
        self.b.get(i..i + p.len()).is_some_and(|x| x.eq_ignore_ascii_case(p.as_bytes()))
    }

    /// The token at `i` and where it ends.
    fn token(&self, i: usize) -> (Tok, usize) {
        let c = self.at(i);
        let n = self.at(i + 1);
        let prev = if i == 0 { 0 } else { self.at(i - 1) };
        match c {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c => (Tok::Space, i + 1),
            b'-' if n == b'-' && (!self.d.dash_comment_space || {
                let c = self.at(i + 2);
                c.is_ascii_whitespace() || c.is_ascii_control()
            }) =>
            {
                (Tok::LineComment, self.line_end(i))
            }
            b'#' if self.d.hash_comments => (Tok::LineComment, self.line_end(i)),
            b'/' if n == b'*' => (Tok::BlockComment, self.block_comment_end(i)),
            b'\'' => (Tok::Quoted, self.quote_end(i + 1, b'\'', self.d.backslash_escapes)),
            b'"' => (Tok::Quoted, self.quote_end(i + 1, b'"', self.d.backslash_escapes && !self.d.dquote_idents)),
            b'`' if self.d.backtick_idents => (Tok::Quoted, self.quote_end(i + 1, b'`', false)),
            b'[' if self.d.bracket_idents => (Tok::Quoted, self.quote_end(i + 1, b']', false)),
            b'$' if self.d.dollar_quotes && !is_ident(prev) => match self.dollar_tag(i) {
                Some(tag_end) => (Tok::Quoted, self.dollar_end(i, tag_end)),
                None => (Tok::Punct, i + 1),
            },
            _ if is_ident(c) => {
                if let Some(end) = self.prefixed_quote(i) {
                    return (Tok::Quoted, end);
                }
                let mut j = i;
                while j < self.b.len() && is_ident(self.b[j]) {
                    j += 1;
                }
                (Tok::Word, j)
            }
            _ => (Tok::Punct, self.char_end(i)),
        }
    }

    fn char_end(&self, i: usize) -> usize {
        let mut j = i + 1;
        while j < self.b.len() && !self.s.is_char_boundary(j) {
            j += 1;
        }
        j
    }

    /// The newline (not included) or the end.
    fn line_end(&self, i: usize) -> usize {
        self.b[i..].iter().position(|&c| c == b'\n').map_or(self.b.len(), |p| i + p)
    }

    fn block_comment_end(&self, i: usize) -> usize {
        let mut depth = 0usize;
        let mut j = i;
        while j < self.b.len() {
            if self.at(j) == b'/' && self.at(j + 1) == b'*' {
                if depth == 0 || self.d.nested_comments {
                    depth += 1;
                }
                j += 2;
            } else if self.at(j) == b'*' && self.at(j + 1) == b'/' {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return j;
                }
            } else {
                j += 1;
            }
        }
        self.b.len()
    }

    /// After a closing `q` (a doubled one escapes it), from `j` inside.
    fn quote_end(&self, mut j: usize, q: u8, backslash: bool) -> usize {
        while j < self.b.len() {
            let c = self.b[j];
            if backslash && c == b'\\' {
                j += 2;
            } else if c == q {
                if self.at(j + 1) == q {
                    j += 2;
                } else {
                    return j + 1;
                }
            } else {
                j += 1;
            }
        }
        self.b.len()
    }

    /// `E'…'` (with dollar quotes on) and Oracle's `q'…'` / `nq'…'`.
    fn prefixed_quote(&self, i: usize) -> Option<usize> {
        let prev = if i == 0 { 0 } else { self.at(i - 1) };
        if is_ident(prev) {
            return None;
        }
        let c = self.at(i).to_ascii_lowercase();
        if self.d.dollar_quotes && c == b'e' && self.at(i + 1) == b'\'' {
            return Some(self.quote_end(i + 2, b'\'', true));
        }
        if self.d.q_quotes {
            let q = if c == b'q' { i } else if c == b'n' && self.at(i + 1).eq_ignore_ascii_case(&b'q') { i + 1 } else { return None };
            if self.at(q + 1) != b'\'' || q + 2 >= self.b.len() {
                return None;
            }
            let open = self.at(q + 2);
            if open.is_ascii_whitespace() || open >= 0x80 {
                return None;
            }
            let close = match open {
                b'[' => b']',
                b'(' => b')',
                b'{' => b'}',
                b'<' => b'>',
                o => o,
            };
            let mut j = q + 3;
            while j + 1 < self.b.len() {
                if self.b[j] == close && self.b[j + 1] == b'\'' {
                    return Some(j + 2);
                }
                j += 1;
            }
            return Some(self.b.len());
        }
        None
    }

    /// `$tag$` at `i`: where the opening tag ends.
    fn dollar_tag(&self, i: usize) -> Option<usize> {
        let mut j = i + 1;
        if self.at(j).is_ascii_digit() {
            return None; // $1: a parameter
        }
        while j < self.b.len() && (self.b[j].is_ascii_alphanumeric() || self.b[j] == b'_' || self.b[j] >= 0x80) {
            j += 1;
        }
        (self.at(j) == b'$').then_some(j + 1)
    }

    fn dollar_end(&self, i: usize, tag_end: usize) -> usize {
        let tag = &self.s[i..tag_end];
        self.s[tag_end..].find(tag).map_or(self.b.len(), |p| tag_end + p + tag.len())
    }
}

/// A word of a statement and its parenthesis depth.
struct Word<'a> {
    text: &'a str,
    /// Byte offset in the text the words were read from.
    start: usize,
    depth: u32,
    /// The punctuation right before it (comments and spaces skipped), or 0
    /// after a word, a quoted name or string, or at the start.
    prev: u8,
}

impl Word<'_> {
    fn is(&self, k: &str) -> bool {
        self.text.eq_ignore_ascii_case(k)
    }

    /// `k` as a keyword, not a variable or a qualified name (`@case`,
    /// `t.end`, `:end`, `#begin`).
    fn keyword(&self, k: &str) -> bool {
        self.is(k) && !matches!(self.prev, b'@' | b'.' | b':' | b'#')
    }
}

/// Whether the word at `i..end` of `b` can be a keyword: not a variable,
/// parameter or part of a qualified name (`@begin`, `:end`, `$case`,
/// `#begin`, `old.end`, `begin.x`). Quoted identifiers are never words.
/// A `:` right after a name ends a label (`lbl:begin`, MySQL and SQL PL),
/// so the word after it still counts.
fn keyword_at(b: &[u8], i: usize, end: usize) -> bool {
    let prev = if i == 0 { 0 } else { b[i - 1] };
    let next = b.get(end).copied().unwrap_or(0);
    !is_ident(prev) && !matches!(prev, b'@' | b'.' | b'$' | b'#') && !(prev == b':' && !label_colon(b, i - 1)) && next != b'.'
}

/// Whether the `:` at `at` ends a label: it follows a name, not a space or
/// punctuation (`:end`, `::end` are binds and casts).
fn label_colon(b: &[u8], at: usize) -> bool {
    at > 0 && is_ident(b[at - 1])
}

/// The words of `text` outside strings and comments.
fn words<'a>(text: &'a str, d: &ScriptDialect) -> Vec<Word<'a>> {
    let sc = Scanner { s: text, b: text.as_bytes(), d: *d };
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut prev = 0u8;
    let mut i = 0;
    while i < text.len() {
        let (tok, end) = sc.token(i);
        match tok {
            Tok::Word => {
                out.push(Word { text: &text[i..end], start: i, depth, prev });
                prev = 0;
            }
            Tok::Quoted => prev = 0,
            Tok::Punct => {
                let c = text.as_bytes()[i];
                if c == b'(' {
                    depth += 1;
                } else if c == b')' {
                    depth = depth.saturating_sub(1);
                }
                prev = if c == b':' && label_colon(text.as_bytes(), i) { 0 } else { c };
            }
            Tok::Space | Tok::LineComment | Tok::BlockComment => {}
        }
        i = end.max(i + 1);
    }
    out
}

/// Modifiers between `CREATE` and the kind of a PL/SQL unit.
const PLSQL_MODIFIERS: &[&str] = &["or", "replace", "editionable", "noneditionable", "editioning", "and", "compile", "resolve", "noforce", "force"];

/// The block a statement is, decided from its first words as they're read
/// (once per statement, so splitting stays linear in the script's size),
/// and how many `BEGIN … END` / `CASE … END` are open.
#[derive(Default)]
struct Track<'a> {
    /// Its first words (up to 12), as written.
    head: Vec<&'a str>,
    depth: i32,
    /// A block `BEGIN` was opened (a routine body, not `BEGIN TRAN`).
    begins: bool,
    /// A `BEGIN` waits for the next word: `BEGIN TRAN` doesn't open a block.
    pending_begin: bool,
    /// An `END` waits for the next word on its line: `END IF`, `END LOOP`…
    /// close something that never counted; `END CASE` closes a `CASE`.
    pending_end: bool,
}

const HEAD_WORDS: usize = 12;

impl<'a> Track<'a> {
    fn head_is(&self, k: usize, w: &str) -> bool {
        self.head.get(k).is_some_and(|h| h.eq_ignore_ascii_case(w))
    }

    fn head_any(&self, k: usize, ws: &[&str]) -> bool {
        ws.iter().any(|w| self.head_is(k, w))
    }

    /// A word of the statement. `keyword`: it can be one (see
    /// [`keyword_at`]); `@begin` or `old.end` open and close nothing.
    /// `end_closers`: `END IF` & co. exist (not in T-SQL, where the word
    /// after `END` is the next statement).
    fn word(&mut self, w: &'a str, keyword: bool, end_closers: bool) {
        if self.head.len() < HEAD_WORDS {
            self.head.push(w);
        }
        let is = |k: &str| w.eq_ignore_ascii_case(k);
        if self.pending_begin {
            self.pending_begin = false;
            if !["tran", "transaction", "work", "distributed", "dialog", "conversation"].iter().any(|k| is(k)) {
                self.open();
            }
        }
        if self.pending_end {
            self.pending_end = false;
            if end_closers && ["if", "loop", "while", "repeat", "for", "case"].iter().any(|k| is(k)) {
                // END IF closes an IF (never counted); END CASE its CASE.
                if is("case") {
                    self.close();
                }
                return;
            }
            self.close();
        }
        if !keyword {
            return;
        }
        if is("begin") {
            self.pending_begin = true;
        } else if is("case") {
            self.depth += 1;
        } else if is("end") {
            self.pending_end = true;
        }
    }

    /// Anything but a word on the same line: a newline, punctuation, a
    /// string. `terminator`: the statement's terminator (`BEGIN;` is a
    /// transaction).
    fn other(&mut self, terminator: bool) {
        if std::mem::take(&mut self.pending_begin) && !terminator {
            self.open();
        }
        if std::mem::take(&mut self.pending_end) {
            self.close();
        }
    }

    fn open(&mut self) {
        self.depth += 1;
        self.begins = true;
    }

    fn close(&mut self) {
        self.depth = (self.depth - 1).max(0);
    }

    /// A PL/SQL unit, which ends at a `/` line rather than at `;`.
    fn plsql(&self) -> bool {
        if self.head_any(0, &["declare", "begin"]) {
            return true;
        }
        if !self.head_is(0, "create") {
            return false;
        }
        for k in 1..self.head.len().min(8) {
            if self.head_any(k, PLSQL_MODIFIERS) {
                continue;
            }
            return self.head_any(k, &["procedure", "function", "package", "trigger", "type", "library", "java"]);
        }
        false
    }

    /// A routine or trigger definition, whose `BEGIN … END` body holds `;`:
    /// `CREATE|ALTER [OR REPLACE|OR ALTER] [TEMP] [DEFINER = …]
    /// [ALGORITHM = …] [SQL SECURITY …] [AGGREGATE|CONSTRAINT] <kind>`, the
    /// kind right after the modifiers (`create table event (…)` isn't one).
    fn compound(&self) -> bool {
        const KINDS: &[&str] = &["trigger", "procedure", "function", "event"];
        if !self.head_any(0, &["create", "alter"]) {
            return false;
        }
        let mut k = 1;
        loop {
            if self.head_any(k, &["or", "replace", "alter", "temp", "temporary", "aggregate", "constraint"]) {
                k += 1;
            } else if self.head_is(k, "definer") {
                // DEFINER = user[@host]: quoted parts aren't words; skip the
                // bare ones (CURRENT_USER, root, localhost).
                k += 1;
                let limit = k + 3;
                while k < limit && k < self.head.len() && !self.head_any(k, KINDS) {
                    k += 1;
                }
            } else if self.head_is(k, "algorithm") {
                k += 2; // ALGORITHM = UNDEFINED
            } else if self.head_is(k, "sql") && self.head_is(k + 1, "security") {
                k += 3; // SQL SECURITY DEFINER|INVOKER
            } else {
                return self.head_any(k, KINDS);
            }
        }
    }

    /// A T-SQL routine, trigger or view: runs to the end of its batch.
    fn tsql_body(&self) -> bool {
        if !self.head_any(0, &["create", "alter"]) {
            return false;
        }
        let mut k = 1;
        while self.head_any(k, &["or", "alter"]) {
            k += 1;
        }
        self.head_any(k, &["proc", "procedure", "function", "trigger", "view"])
    }
}

struct Splitter<'a> {
    sc: Scanner<'a>,
    term: String,
    newlines: Vec<usize>,
    out: Vec<ScriptStatement>,
    /// Where the statement being read starts (its first token).
    start: Option<usize>,
    /// Its block state.
    track: Track<'a>,
}

impl<'a> Splitter<'a> {
    fn new(s: &'a str, d: ScriptDialect) -> Self {
        let newlines = s.bytes().enumerate().filter(|(_, c)| *c == b'\n').map(|(i, _)| i).collect();
        Self { sc: Scanner { s, b: s.as_bytes(), d }, term: ";".into(), newlines, out: Vec::new(), start: None, track: Track::default() }
    }

    fn line_of(&self, offset: usize) -> u32 {
        self.newlines.partition_point(|&n| n < offset) as u32 + 1
    }

    fn run(mut self) -> Vec<ScriptStatement> {
        let len = self.sc.b.len();
        let end_closers = !self.sc.d.tsql_blocks;
        let mut i = 0;
        while i < len {
            if i == 0 || self.sc.b[i - 1] == b'\n' {
                if let Some(next) = self.line_directive(i) {
                    i = next;
                    continue;
                }
            }
            let (tok, end) = self.sc.token(i);
            match tok {
                Tok::Space => {
                    if self.sc.b[i] == b'\n' {
                        self.track.other(false);
                    }
                }
                Tok::LineComment => {
                    if self.sc.d.terminator_directive {
                        self.terminator_directive(i, end);
                    }
                }
                Tok::BlockComment => {
                    // MySQL's versioned comments run.
                    if self.start.is_none() && (self.sc.starts(i, "/*!") || self.sc.starts(i, "/*M!")) {
                        self.start = Some(i);
                    }
                }
                Tok::Punct if self.sc.d.semicolons && self.sc.s[i..].starts_with(self.term.as_str()) => {
                    self.track.other(true);
                    if !self.holds_terminator() {
                        let kind = self.kind_at_end();
                        self.finish(i, kind, 1);
                        i += self.term.len();
                        continue;
                    }
                    self.start.get_or_insert(i);
                }
                _ => {
                    // A switched terminator may be made of word characters
                    // (`$$`, `@@`) and end a word (`END$$`).
                    if self.sc.d.semicolons && self.term != ";" {
                        let term = self.term.clone();
                        let hit = if tok == Tok::Word { self.sc.s[i..end].find(term.as_str()) } else { self.sc.s[i..].starts_with(term.as_str()).then_some(0) };
                        if let Some(p) = hit {
                            let at = i + p;
                            if p > 0 {
                                self.start.get_or_insert(i);
                                let keyword = keyword_at(self.sc.b, i, at);
                                self.track.word(&self.sc.s[i..at], keyword, end_closers);
                            }
                            self.track.other(true);
                            if !self.holds_terminator() {
                                let kind = self.kind_at_end();
                                self.finish(at, kind, 1);
                                i = at + term.len();
                                continue;
                            }
                            // Held: the rest of the token is read as is.
                            self.start.get_or_insert(i);
                            i = end.max(i + 1);
                            continue;
                        }
                    }
                    self.start.get_or_insert(i);
                    if tok == Tok::Word {
                        let keyword = keyword_at(self.sc.b, i, end);
                        self.track.word(&self.sc.s[i..end], keyword, end_closers);
                    } else {
                        self.track.other(false);
                    }
                }
            }
            i = end.max(i + 1);
        }
        self.track.other(true);
        let kind = self.kind_at_end();
        self.finish(len, kind, 1);
        self.out
    }

    /// The statement read so far is a block still open at a terminator.
    fn holds_terminator(&self) -> bool {
        if self.start.is_none() {
            return false;
        }
        let (d, t) = (&self.sc.d, &self.track);
        (d.plsql_blocks && t.plsql()) || (d.compound_blocks && t.depth > 0 && t.compound()) || (d.tsql_blocks && (t.depth > 0 || t.tsql_body()))
    }

    fn kind_at_end(&self) -> StatementKind {
        let (d, t) = (&self.sc.d, &self.track);
        if self.start.is_some() && d.batch == BatchLine::Go && !d.semicolons {
            StatementKind::Batch
        } else if self.start.is_some()
            && ((d.plsql_blocks && t.plsql()) || (d.compound_blocks && t.begins && t.compound()) || (d.tsql_blocks && t.tsql_body()))
        {
            StatementKind::Block
        } else {
            StatementKind::Sql
        }
    }

    /// End the statement being read at `end` (exclusive).
    fn finish(&mut self, end: usize, kind: StatementKind, repeat: u32) {
        self.track = Track::default();
        let Some(st) = self.start.take() else { return };
        let text = self.sc.s[st..end].trim_end();
        if text.is_empty() {
            return;
        }
        let mut kind = kind;
        if self.sc.d.set_term {
            let ws = words(text, &self.sc.d);
            if ws.len() >= 2 && ws[0].is("set") && ws[1].is("term") {
                // The new terminator is the first token after TERM.
                let after = ws[1].start + ws[1].text.len();
                if let Some(t) = text[after..].split_whitespace().next() {
                    self.term = t.to_string();
                    kind = StatementKind::ClientCommand;
                }
            }
        }
        self.out.push(ScriptStatement { text: text.to_string(), start: st, end: st + text.len(), line: self.line_of(st), kind, repeat, error: None });
    }

    /// A whole-line directive at line start `i`: where the next line starts.
    fn line_directive(&mut self, i: usize) -> Option<usize> {
        let line_end = self.sc.line_end(i);
        let next = (line_end + 1).min(self.sc.b.len());
        let line = &self.sc.s[i..line_end];
        let trimmed = line.trim();
        match self.sc.d.batch {
            BatchLine::Go => {
                if let Some(repeat) = go_line(trimmed) {
                    self.track.other(true);
                    let kind = self.kind_at_end();
                    self.finish(i, kind, repeat.unwrap_or(1));
                    if repeat.is_none() {
                        // `GO 99999999999`: a unit of its own that carries
                        // the error, so the app reports it at this line.
                        let st = i + (line.len() - line.trim_start().len());
                        self.out.push(ScriptStatement {
                            text: trimmed.to_string(),
                            start: st,
                            end: st + trimmed.len(),
                            line: self.line_of(st),
                            kind: StatementKind::Batch,
                            repeat: 1,
                            error: Some(GO_COUNT_ERROR.to_string()),
                        });
                    }
                    return Some(next);
                }
            }
            BatchLine::Slash => {
                if trimmed == "/" {
                    self.track.other(true);
                    let kind = self.kind_at_end();
                    self.finish(i, kind, 1);
                    return Some(next);
                }
            }
            BatchLine::None => {}
        }
        if self.sc.d.delimiter_command && self.start.is_none() {
            let mut parts = trimmed.split_whitespace();
            if parts.next().is_some_and(|w| w.eq_ignore_ascii_case("delimiter")) {
                if let Some(t) = parts.next() {
                    self.term = t.to_string();
                    let st = i + (line.len() - line.trim_start().len());
                    self.out.push(ScriptStatement {
                        text: trimmed.to_string(),
                        start: st,
                        end: st + trimmed.len(),
                        line: self.line_of(st),
                        kind: StatementKind::ClientCommand,
                        repeat: 1,
                        error: None,
                    });
                    return Some(next);
                }
            }
        }
        None
    }

    /// `--#SET TERMINATOR x` in the comment at `i..end`.
    fn terminator_directive(&mut self, i: usize, end: usize) {
        let body = self.sc.s[i + 2..end].trim_start();
        let Some(rest) = body.strip_prefix('#') else { return };
        let mut parts = rest.split_whitespace();
        if parts.next().is_some_and(|w| w.eq_ignore_ascii_case("set")) && parts.next().is_some_and(|w| w.eq_ignore_ascii_case("terminator")) {
            if let Some(t) = parts.next() {
                self.term = t.to_string();
            }
        }
    }
}

/// `GO`, `GO 5`, `go -- x`, `GO 3 /* x */`: the repeat count, or
/// `Some(None)` when the count isn't a valid one (`GO 99999999999`: SSMS
/// refuses it, above 2147483647).
fn go_line(line: &str) -> Option<Option<u32>> {
    let b = line.as_bytes();
    if b.len() < 2 || !b[..2].eq_ignore_ascii_case(b"go") {
        return None;
    }
    let mut rest = line[2..].trim_start();
    if rest.len() == line.len() - 2 && !rest.is_empty() && !rest.starts_with("--") && !rest.starts_with("/*") {
        return None; // GOTO, GO;, GOx
    }
    // A sign only before a digit: `GO --x` is a comment.
    let sign = usize::from(rest.starts_with('-') && rest[1..].starts_with(|c: char| c.is_ascii_digit()));
    let digits = sign + rest[sign..].bytes().take_while(u8::is_ascii_digit).count();
    // SSMS rejects 0, negatives and counts past i32 (`None`: client error).
    let repeat = if digits == 0 { Some(1) } else { rest[..digits].parse::<i32>().ok().filter(|&n| n >= 1).map(|n| n as u32) };
    rest = rest[digits..].trim_start();
    if rest.is_empty() || rest.starts_with("--") {
        return Some(repeat);
    }
    if let Some(c) = rest.strip_prefix("/*") {
        if let Some(p) = c.find("*/") {
            if c[p + 2..].trim().is_empty() || c[p + 2..].trim_start().starts_with("--") {
                return Some(repeat);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str, d: ScriptDialect) -> Vec<String> {
        split_script(s, &d).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn generic_splits_on_semicolons_outside_quotes_and_comments() {
        let s = "select ';' ; -- x;\nselect \"a;b\", `c;d` from t; /* ; */ ;";
        assert_eq!(texts(s, ScriptDialect::generic()), vec!["select ';'", "select \"a;b\", `c;d` from t"]);
    }

    #[test]
    fn doubled_quotes_stay_inside() {
        assert_eq!(texts("select 'it''s; fine'; select 2", ScriptDialect::generic()), vec!["select 'it''s; fine'", "select 2"]);
    }

    #[test]
    fn offsets_and_lines_point_into_the_script() {
        let s = "-- head\n\n  select 1;\nselect\n  2 ;  \n";
        let st = split_script(s, &ScriptDialect::generic());
        assert_eq!(st.len(), 2);
        assert_eq!(&s[st[0].start..st[0].end], "select 1");
        assert_eq!(st[0].line, 3);
        assert_eq!(&s[st[1].start..st[1].end], "select\n  2");
        assert_eq!(st[1].line, 4);
        assert!(st.iter().all(|x| x.kind == StatementKind::Sql && x.repeat == 1));
    }

    #[test]
    fn offsets_are_bytes_with_multibyte_text() {
        let s = "select 'ñandú';\nselect 'é'";
        let st = split_script(s, &ScriptDialect::generic());
        assert_eq!(&s[st[1].start..st[1].end], "select 'é'");
        assert_eq!(st[1].line, 2);
    }

    #[test]
    fn comment_only_pieces_are_dropped() {
        assert!(split_script("-- nothing\n/* here */;;", &ScriptDialect::generic()).is_empty());
    }

    #[test]
    fn dollar_quotes() {
        let d = ScriptDialect::postgres();
        let s = "create function f() returns int as $$ begin; return 1; end $$ language plpgsql;\ndo $body$ select ';' $body$; select $1";
        assert_eq!(
            texts(s, d),
            vec!["create function f() returns int as $$ begin; return 1; end $$ language plpgsql", "do $body$ select ';' $body$", "select $1"]
        );
        // Without the option, $$ is plain text.
        assert_eq!(texts("select $$a;b$$", ScriptDialect::generic()), vec!["select $$a", "b$$"]);
        // A $ inside an identifier doesn't open one.
        assert_eq!(texts("select a$b$ from t; select 2", d), vec!["select a$b$ from t", "select 2"]);
    }

    #[test]
    fn e_strings_and_backslash_escapes() {
        assert_eq!(texts(r"select E'a\'; b'; select 2", ScriptDialect::postgres()), vec![r"select E'a\'; b'", "select 2"]);
        assert_eq!(texts(r"select 'a\'; b'; select 2", ScriptDialect::mysql()), vec![r"select 'a\'; b'", "select 2"]);
        // Standard strings: the backslash is literal.
        assert_eq!(texts(r"select 'a\'; select 2", ScriptDialect::postgres()), vec![r"select 'a\'", "select 2"]);
    }

    #[test]
    fn nested_comments() {
        let s = "select 1 /* a /* b */ ; */; select 2";
        assert_eq!(texts(s, ScriptDialect::postgres()), vec!["select 1 /* a /* b */ ; */", "select 2"]);
        // Not nested: the first */ closes it.
        assert_eq!(texts(s, ScriptDialect::generic()), vec!["select 1 /* a /* b */", "*/", "select 2"]);
    }

    #[test]
    fn hash_comments_in_mysql() {
        assert_eq!(texts("select 1; # a;b\nselect 2", ScriptDialect::mysql()), vec!["select 1", "select 2"]);
    }

    #[test]
    fn versioned_comments_run() {
        let st = split_script("/*!40101 SET NAMES utf8 */;\nselect 1", &ScriptDialect::mysql());
        assert_eq!(st[0].text, "/*!40101 SET NAMES utf8 */");
    }

    #[test]
    fn oracle_q_quotes() {
        let d = ScriptDialect::oracle();
        assert_eq!(texts("select q'[it's; ok]' from dual;\nselect nq'{a;b}' from dual;", d), vec!["select q'[it's; ok]' from dual", "select nq'{a;b}' from dual"]);
        assert_eq!(texts("select q'!x;y!' from dual; select 1 from dual", d), vec!["select q'!x;y!' from dual", "select 1 from dual"]);
    }

    #[test]
    fn brackets_in_tsql() {
        let d = ScriptDialect::tsql().statements();
        assert_eq!(texts("select [a;b]]c] from t; select 2", d), vec!["select [a;b]]c] from t", "select 2"]);
    }

    #[test]
    fn go_batches() {
        let s = "create table t (a int)\ngo\ninsert t values (1);\ninsert t values (2)\nGO 3 -- three times\nselect 1\n  Go  \nselect 2";
        let st = split_script(s, &ScriptDialect::tsql());
        let got: Vec<_> = st.iter().map(|s| (s.text.as_str(), s.kind, s.repeat, s.line)).collect();
        assert_eq!(
            got,
            vec![
                ("create table t (a int)", StatementKind::Batch, 1, 1),
                ("insert t values (1);\ninsert t values (2)", StatementKind::Batch, 3, 3),
                ("select 1", StatementKind::Batch, 1, 6),
                ("select 2", StatementKind::Batch, 1, 8),
            ]
        );
    }

    #[test]
    fn go_lines_with_comments_and_not_go() {
        assert_eq!(go_line("GO"), Some(Some(1)));
        assert_eq!(go_line("go 5"), Some(Some(5)));
        assert_eq!(go_line("GO -- end"), Some(Some(1)));
        assert_eq!(go_line("GO 2 /* x */"), Some(Some(2)));
        assert_eq!(go_line("GO/* x */"), Some(Some(1)));
        assert_eq!(go_line("GO;"), None);
        assert_eq!(go_line("GOTO x"), None);
        assert_eq!(go_line("go x"), None);
        assert_eq!(go_line("GO 0"), Some(None));
        assert_eq!(go_line("GO -5"), Some(None));
        assert_eq!(go_line("GO --5"), Some(Some(1)));
        assert_eq!(go_line("GO-5"), None);
        assert_eq!(go_line("GO 2147483647"), Some(Some(2147483647)));
        // Too big: a GO line all the same, with an invalid count.
        assert_eq!(go_line("GO 99999999999"), Some(None));
        assert_eq!(go_line("GO 2147483648 -- x"), Some(None));
    }

    #[test]
    fn go_inside_comments_and_strings_does_not_split() {
        let s = "select 1 /*\nGO\n*/\nselect 'a\nGO\nb'\nselect [x\ngo\n]\nGO\nselect 2";
        let st = split_script(s, &ScriptDialect::tsql());
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].text, "select 1 /*\nGO\n*/\nselect 'a\nGO\nb'\nselect [x\ngo\n]");
        assert_eq!(st[1].text, "select 2");
        // A trailing GO with nothing after it adds nothing.
        assert_eq!(split_script("select 1\nGO\n", &ScriptDialect::tsql()).len(), 1);
    }

    #[test]
    fn go_with_an_invalid_count_is_a_client_error() {
        let s = "select 1\nGO 99999999999\nselect 2";
        let st = split_script(s, &ScriptDialect::tsql());
        let got: Vec<_> = st.iter().map(|s| (s.text.as_str(), s.line, s.repeat, s.error.as_deref())).collect();
        assert_eq!(got, vec![("select 1", 1, 1, None), ("GO 99999999999", 2, 1, Some(GO_COUNT_ERROR)), ("select 2", 3, 1, None)]);
        assert_eq!(&s[st[1].start..st[1].end], "GO 99999999999");
        // Not folded into the batch, also statement by statement and with
        // nothing before it.
        let st = split_script("GO 99999999999\nselect 2", &ScriptDialect::tsql().statements());
        assert_eq!(st.iter().map(|s| s.error.is_some()).collect::<Vec<_>>(), vec![true, false]);
        // Zero and negative counts too; `GO --5` is still a plain GO.
        for go in ["GO 0", "GO -5", "go -1 -- x"] {
            let st = split_script(&format!("select 1\n{go}\nselect 2"), &ScriptDialect::tsql());
            assert_eq!(st.iter().map(|s| s.error.is_some()).collect::<Vec<_>>(), vec![false, true, false], "{go}");
        }
        let st = split_script("select 1\nGO --5\nselect 2", &ScriptDialect::tsql());
        assert_eq!(st.iter().map(|s| (s.text.as_str(), s.error.is_some())).collect::<Vec<_>>(), vec![("select 1", false), ("select 2", false)]);
        // The field stays off the wire when there's none, and defaults.
        let json = serde_json::to_string(&split_script("select 1", &ScriptDialect::tsql())[0]).unwrap();
        assert!(!json.contains("error"), "{json}");
        let back: ScriptStatement = serde_json::from_str(r#"{"text":"x","start":0,"end":1,"line":1,"kind":"batch"}"#).unwrap();
        assert_eq!((back.repeat, back.error), (1, None));
    }

    #[test]
    fn go_lines_also_split_statement_by_statement() {
        let d = ScriptDialect::tsql().statements();
        assert_eq!(texts("select 1; select 2\nGO\nselect 3", d), vec!["select 1", "select 2", "select 3"]);
    }

    #[test]
    fn delimiter_switches_the_terminator() {
        let s = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END//\nDELIMITER ;\nCALL p();";
        let st = split_script(s, &ScriptDialect::mysql());
        let got: Vec<_> = st.iter().map(|s| (s.text.as_str(), s.kind)).collect();
        assert_eq!(
            got,
            vec![
                ("DELIMITER //", StatementKind::ClientCommand),
                ("CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END", StatementKind::Block),
                ("DELIMITER ;", StatementKind::ClientCommand),
                ("CALL p()", StatementKind::Sql),
            ]
        );
        assert_eq!(st[1].line, 2);
        // $$ as a delimiter (a word-ish terminator).
        let st = texts("delimiter $$\nselect 1$$\nselect 2$$", ScriptDialect::mysql());
        assert_eq!(st, vec!["delimiter $$", "select 1", "select 2"]);
    }

    #[test]
    fn set_term_switches_the_terminator() {
        let s = "SET TERM ^ ;\nCREATE TRIGGER t FOR x BEFORE INSERT AS BEGIN new.a = 1; END^\nSET TERM ; ^\nselect 1 from rdb$database;";
        let st = split_script(s, &ScriptDialect::firebird());
        let got: Vec<_> = st.iter().map(|s| (s.text.as_str(), s.kind)).collect();
        assert_eq!(
            got,
            vec![
                ("SET TERM ^", StatementKind::ClientCommand),
                ("CREATE TRIGGER t FOR x BEFORE INSERT AS BEGIN new.a = 1; END", StatementKind::Block),
                ("SET TERM ;", StatementKind::ClientCommand),
                ("select 1 from rdb$database", StatementKind::Sql),
            ]
        );
    }

    #[test]
    fn db2_terminator_directive() {
        let s = "--#SET TERMINATOR @\nCREATE PROCEDURE p() BEGIN DECLARE x INT; SET x = 1; END@\n--#SET TERMINATOR ;\nselect 1 from sysibm.sysdummy1;";
        assert_eq!(
            texts(s, ScriptDialect { compound_blocks: false, ..ScriptDialect::db2() }),
            vec!["CREATE PROCEDURE p() BEGIN DECLARE x INT; SET x = 1; END", "select 1 from sysibm.sysdummy1"]
        );
    }

    #[test]
    fn oracle_blocks_end_at_a_slash_line() {
        let s = "create table t (a number);\nCREATE OR REPLACE PROCEDURE p IS\nBEGIN\n  insert into t values (1);\nEND;\n/\nbegin p; end;\n/\nselect * from t\n/\nselect 1 from dual;";
        let st = split_script(s, &ScriptDialect::oracle());
        let got: Vec<_> = st.iter().map(|s| (s.text.as_str(), s.kind, s.line)).collect();
        assert_eq!(
            got,
            vec![
                ("create table t (a number)", StatementKind::Sql, 1),
                ("CREATE OR REPLACE PROCEDURE p IS\nBEGIN\n  insert into t values (1);\nEND;", StatementKind::Block, 2),
                ("begin p; end;", StatementKind::Block, 7),
                ("select * from t", StatementKind::Sql, 9),
                ("select 1 from dual", StatementKind::Sql, 11),
            ]
        );
        // A block without its slash runs to the end.
        let st = split_script("declare x number; begin x := 1; end;", &ScriptDialect::oracle());
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].kind, StatementKind::Block);
        // A / that isn't alone on its line is division.
        assert_eq!(texts("select 4\n/ 2 from dual;", ScriptDialect::oracle()), vec!["select 4\n/ 2 from dual"]);
    }

    #[test]
    fn trigger_bodies_hold_semicolons() {
        let s = "CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE u SET n = CASE WHEN 1 THEN 2 END; INSERT INTO l VALUES (1); END;\nselect 1;";
        let st = split_script(s, &ScriptDialect::generic());
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].kind, StatementKind::Block);
        assert!(st[0].text.ends_with("END"));
        // A PostgreSQL trigger has no body: it ends at its ;.
        assert_eq!(texts("create trigger x after insert on t execute function f(); select 1", ScriptDialect::postgres()).len(), 2);
        // BEGIN TRANSACTION isn't a block, nor is BEGIN; on its own.
        assert_eq!(texts("BEGIN; update t set a = 1 where b = 2; COMMIT;", ScriptDialect::generic()).len(), 3);
        // MySQL routines with IF … END IF.
        let s = "create procedure p() begin if 1 then select 1; end if; end; select 2";
        assert_eq!(texts(s, ScriptDialect::mysql()), vec!["create procedure p() begin if 1 then select 1; end if; end", "select 2"]);
    }

    #[test]
    fn unclosed_quotes_run_to_the_end() {
        assert_eq!(texts("select 'abc; select 2", ScriptDialect::generic()), vec!["select 'abc; select 2"]);
    }

    #[test]
    fn strip_comments_keeps_hints_on_demand() {
        let d = ScriptDialect::mysql();
        assert_eq!(strip_comments("select /*+ X */ 1 -- c\n, 2 /* y */", &d, true), "select /*+ X */ 1 \n, 2  ");
        assert_eq!(strip_comments("select /*+ X */ 1 # c\n", &d, false), "select   1 \n");
    }

    #[test]
    fn leading_keywords() {
        assert_eq!(leading_keyword("  /* x */ -- y\n Select 1", &ScriptDialect::generic()).as_deref(), Some("select"));
        assert_eq!(leading_keyword("(select 1)", &ScriptDialect::generic()).as_deref(), Some("select"));
    }

    #[test]
    fn update_and_delete_without_where() {
        let g = ScriptDialect::generic();
        assert_eq!(unsafe_dml("delete from t", &g), Some("DELETE"));
        assert_eq!(unsafe_dml("DELETE FROM t WHERE id = 1", &g), None);
        assert_eq!(unsafe_dml("update t set a = (select max(b) from u where u.x = 1)", &g), Some("UPDATE"));
        assert_eq!(unsafe_dml("update t set a = 1 -- where x\n", &g), Some("UPDATE"));
        assert_eq!(unsafe_dml("update t set a = 'where'", &g), Some("UPDATE"));
        assert_eq!(unsafe_dml("delete from t limit 10", &g), Some("DELETE"));
        assert_eq!(unsafe_dml("with x as (select 1 where true) delete from t", &g), Some("DELETE"));
        assert_eq!(unsafe_dml("with x as (select 1) update t set a = 1 from x where t.id = x.id", &g), None);
        assert_eq!(unsafe_dml("UPDATE STATISTICS t", &g), None);
        assert_eq!(unsafe_dml("select * from t", &g), None);
        assert_eq!(unsafe_dml("delete from t where current of c", &g), None);
    }

    #[test]
    fn unsafe_statements_in_a_script() {
        let s = "select 1;\ndelete from t;\nupdate u set a = 1 where b = 2;\nCREATE TRIGGER x AFTER INSERT ON t BEGIN DELETE FROM l; END;";
        let u = unsafe_statements(s, &ScriptDialect::generic());
        assert_eq!(u.len(), 1);
        assert_eq!((u[0].keyword.as_str(), u[0].line), ("DELETE", 2));
        assert_eq!(&s[u[0].start..u[0].end], "delete from t");
        // T-SQL: looked at statement by statement inside a batch.
        let u = unsafe_statements("begin tran; update t set a = 1; commit\nGO\ndelete t where a = 1", &ScriptDialect::tsql());
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].keyword, "UPDATE");
    }

    #[test]
    fn end_case_and_end_on_the_next_line_close_their_block() {
        // MySQL CASE statement: END CASE closes the CASE.
        let s = "create procedure p(x int) begin case x when 1 then select 1; else select 2; end case; end;\ndelete from t;\nselect 3;";
        assert_eq!(
            texts(s, ScriptDialect::mysql()),
            vec!["create procedure p(x int) begin case x when 1 then select 1; else select 2; end case; end", "delete from t", "select 3"]
        );
        assert_eq!(unsafe_statements(s, &ScriptDialect::mysql()).len(), 1);
        // An END ending its line closes a block: the IF after it is the
        // next statement's, not END IF.
        let s = "CREATE PROCEDURE p AS BEGIN SELECT 1; END\n IF @b=1 SELECT 2; END; SELECT 3; DELETE FROM t;";
        let st = texts(s, ScriptDialect::generic());
        assert_eq!(st.last().map(String::as_str), Some("DELETE FROM t"));
        assert_eq!(st.len(), 4);
        assert_eq!(unsafe_statements(s, &ScriptDialect::generic()).len(), 1);
        // END; IF … : the ; ends the END too.
        let s = "create trigger tr after insert on t begin select 1; end; if x then delete from t; end if";
        assert_eq!(texts(s, ScriptDialect::generic())[0], "create trigger tr after insert on t begin select 1; end");
        // END IF, END LOOP, END WHILE on one line stay inside the routine.
        let s = "create procedure p() begin while 1 do loop leave; end loop; end while; repeat select 1; until 1 end repeat; end; select 2";
        assert_eq!(texts(s, ScriptDialect::mysql()).len(), 2);
    }

    #[test]
    fn tsql_routines_run_to_the_end_of_their_batch() {
        let d = ScriptDialect::tsql().statements();
        let s = "CREATE PROCEDURE p AS BEGIN SET NOCOUNT ON; UPDATE t SET a = 1; END\nGO\nselect 1; select 2";
        let st = split_script(s, &d);
        let got: Vec<_> = st.iter().map(|x| (x.text.as_str(), x.kind)).collect();
        assert_eq!(
            got,
            vec![
                ("CREATE PROCEDURE p AS BEGIN SET NOCOUNT ON; UPDATE t SET a = 1; END", StatementKind::Block),
                ("select 1", StatementKind::Sql),
                ("select 2", StatementKind::Sql),
            ]
        );
        // No BEGIN … END, CREATE OR ALTER, triggers: the body's DML isn't asked about.
        let s = "CREATE OR ALTER PROC p AS UPDATE t SET a = 1; DELETE FROM u;\nGO\ncreate trigger tr on t after insert as delete from l;\nGO\nDELETE FROM v";
        let u = unsafe_statements(s, &ScriptDialect::tsql());
        assert_eq!(u.iter().map(|x| &s[x.start..x.end]).collect::<Vec<_>>(), vec!["DELETE FROM v"]);
        // A BEGIN … END keeps its ; inside; its DML is looked into.
        let s = "IF @b = 1 BEGIN DELETE FROM t WHERE a = 1; DELETE FROM u; END\nSELECT 1";
        assert_eq!(texts(s, d).len(), 1);
        let u = unsafe_statements(s, &ScriptDialect::tsql());
        assert_eq!(u.iter().map(|x| &s[x.start..x.end]).collect::<Vec<_>>(), vec!["DELETE FROM u"]);
        // BEGIN TRAN isn't a block.
        assert_eq!(texts("BEGIN TRAN; UPDATE t SET a = 1 WHERE b = 2; COMMIT", d).len(), 3);
        // BEGIN TRY … END TRY BEGIN CATCH … END CATCH.
        assert_eq!(texts("BEGIN TRY SELECT 1; END TRY BEGIN CATCH SELECT 2; END CATCH; SELECT 3", d).len(), 2);
    }

    #[test]
    fn unsafe_dml_without_semicolons() {
        let t = ScriptDialect::tsql();
        // The next statement's WHERE isn't the UPDATE's.
        let s = "UPDATE t SET a = 1\nSELECT * FROM t WHERE id = 1";
        let u = unsafe_statements(s, &t);
        assert_eq!(u.iter().map(|x| (&s[x.start..x.end], x.line)).collect::<Vec<_>>(), vec![("UPDATE t SET a = 1", 1)]);
        // Two statements of a batch, the second one unsafe.
        let s = "UPDATE t SET a = 1 WHERE b = 2\nDELETE FROM u\nGO\nIF EXISTS (SELECT 1 FROM x) DELETE FROM w ELSE DELETE FROM w WHERE a = 1";
        let u = unsafe_statements(s, &t);
        assert_eq!(u.iter().map(|x| (&s[x.start..x.end], x.line)).collect::<Vec<_>>(), vec![("DELETE FROM u", 2), ("DELETE FROM w", 4)]);
        // CASE … END isn't the end of the statement.
        assert_eq!(unsafe_dml("UPDATE t SET a = CASE WHEN x THEN 1 ELSE 2 END WHERE id = 1", &t), None);
        // SQL++ USE KEYS limits a write as a WHERE does.
        let g = ScriptDialect::generic();
        assert_eq!(unsafe_dml("UPDATE ks USE KEYS 'h1' SET v = 2", &g), None);
        assert_eq!(unsafe_dml("DELETE FROM ks USE KEYS ['a', 'b']", &g), None);
        assert_eq!(unsafe_dml("DELETE FROM ks", &g), Some("DELETE"));
        // GO lines split under the generic dialect's guard? No (no GO there),
        // but T-SQL's does.
        assert_eq!(unsafe_statements("select 1\nGO\ndelete from t", &t).len(), 1);
    }

    #[test]
    fn unsafe_dml_in_ctes_and_not_dml_at_all() {
        let g = ScriptDialect::postgres();
        assert_eq!(unsafe_dml("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", &g), Some("DELETE"));
        assert_eq!(unsafe_dml("WITH d AS (DELETE FROM t WHERE a = 1 RETURNING *) SELECT * FROM d", &g), None);
        assert_eq!(unsafe_dml("with u as materialized (update t set a = 1 returning id) select 1", &g), Some("UPDATE"));
        let s = "with x as (select 1) delete from t";
        let u = unsafe_statements(s, &g);
        assert_eq!(&s[u[0].start..u[0].end], s, "the WITH is part of it");
        for ok in [
            "select * from t for update",
            "select * from t for update of t skip locked",
            "insert into t values (1) on duplicate key update a = 1",
            "insert into t values (1) on conflict (a) do update set b = 1",
            "merge into t using s on t.id = s.id when matched and s.x then delete when matched then update set a = 1",
            "grant select, update on t to u",
            "grant update on t to u",
            "revoke delete on t from u",
            "create table c (a int references p(id) on delete cascade on update set null)",
            "create trigger tr before insert or update of a on t for each row execute function f()",
            "create policy p on t for delete using (true)",
            "create rule r as on delete to t do instead nothing",
            "select 1 union select 2",
        ] {
            assert_eq!(unsafe_dml(ok, &g), None, "{ok}");
        }
        // A dollar-quoted body is one statement and not looked into.
        let s = "create function f() returns void as $$ begin delete from log; update t set a = 1; end $$ language plpgsql;\nselect 1";
        assert!(unsafe_statements(s, &g).is_empty());
        assert_eq!(split_script(s, &g.statements()).len(), 2);
        // MySQL's versioned comments run.
        assert_eq!(unsafe_statements("select 1; /*!50000 delete from t */;", &ScriptDialect::mysql()).len(), 1);
    }

    #[test]
    fn mysql_double_dash_needs_a_space() {
        let m = ScriptDialect::mysql();
        assert_eq!(texts("update t set a = a--1 where id = 1; delete from u where x = 1", m), vec!["update t set a = a--1 where id = 1", "delete from u where x = 1"]);
        assert!(unsafe_statements("update t set a = a--1 where id = 1", &m).is_empty());
        assert_eq!(texts("select 1 -- c;\n;select 2 --\tx;\n", m), vec!["select 1 -- c;", "select 2 --\tx;"]);
        assert_eq!(strip_comments("select 1 --", &m, false), "select 1 \n");
        // Elsewhere -- is always a comment.
        assert_eq!(texts("select 1--1; select 2", ScriptDialect::postgres()), vec!["select 1--1; select 2"]);
    }

    #[test]
    fn variables_and_qualified_names_are_not_block_keywords() {
        // T-SQL: @begin is a variable, not a BEGIN block.
        let s = "DECLARE @begin datetime2 = SYSDATETIME(); SELECT * FROM t; UPDATE t SET a = 1 WHERE id = 2; SELECT DATEDIFF(ms, @begin, SYSDATETIME());";
        assert_eq!(texts(s, ScriptDialect::tsql().statements()).len(), 4);
        assert!(unsafe_statements(s, &ScriptDialect::tsql()).is_empty());
        let s = "SET @case = 1; SELECT #end.a FROM #end; SELECT [begin], \"end\" FROM t; DELETE FROM u";
        assert_eq!(texts(s, ScriptDialect::tsql().statements()).len(), 4);
        // MySQL: user variables and NEW./OLD. columns inside a routine and a
        // trigger don't move the block's depth.
        let m = ScriptDialect::mysql();
        let s = "create procedure p() begin set @begin = now(); select @begin, @end; end; select 2";
        assert_eq!(texts(s, m), vec!["create procedure p() begin set @begin = now(); select @begin, @end; end", "select 2"]);
        let s = "create trigger tr before update on t for each row begin if new.begin > old.end then set new.`case` = 1; set new.case = 2; end if; end;\ndelete from t;";
        let st = split_script(s, &m);
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].kind, StatementKind::Block);
        assert_eq!(st[1].text, "delete from t");
        assert_eq!(unsafe_statements(s, &m).len(), 1);
        // `begin` as a backtick name and a qualifier: not a block either.
        assert_eq!(texts("select `begin` from t; select begin.x from t begin; select 3", m).len(), 3);
        // :end (a bind) and $begin aren't keywords either.
        let o = ScriptDialect::generic();
        assert_eq!(texts("create trigger tr after insert on t begin select :end; select 1; end; select 2", o).len(), 2);
        // A label right before BEGIN (no space) is still a block.
        let s = "create trigger tr before insert on t for each row b:begin set new.a=1; set new.b=2; end b; select 3";
        assert_eq!(texts(s, m), vec!["create trigger tr before insert on t for each row b:begin set new.a=1; set new.b=2; end b", "select 3"]);
        let s = "create procedure p() begin outer_l:begin select 1; end outer_l; select 2; end; select 3";
        assert_eq!(texts(s, m), vec!["create procedure p() begin outer_l:begin select 1; end outer_l; select 2; end", "select 3"]);
        let s = "CREATE PROCEDURE p() L1:BEGIN DECLARE x INT; SET x=1; END L1; VALUES 1";
        assert_eq!(texts(s, ScriptDialect::db2()), vec!["CREATE PROCEDURE p() L1:BEGIN DECLARE x INT; SET x=1; END L1", "VALUES 1"]);
        assert_eq!(texts("lbl:BEGIN SELECT 1; SELECT 2; END", ScriptDialect::tsql().statements()).len(), 1);
        // After a space or punctuation it's still a bind or a cast.
        assert_eq!(texts("create trigger tr after insert on t begin select x::end; select 1; end; select 2", o).len(), 2);
        assert_eq!(texts("create trigger tr after insert on t begin select (:end); select 1; end; select 2", o).len(), 2);
        // A CASE on a variable in the WHERE guard.
        assert_eq!(unsafe_dml("update t set a = @case where id = 1", &ScriptDialect::tsql()), None);
    }

    #[test]
    fn compound_headers_need_the_create_kind_shape() {
        let m = ScriptDialect::mysql();
        // A table named event with a column named begin isn't a routine.
        assert_eq!(texts("create table event (id int, begin int); insert into event values (1, 2)", m).len(), 2);
        assert_eq!(texts("create table trigger_log (begin int, procedure int); select 1", ScriptDialect::generic()).len(), 2);
        // The real shapes still hold their body.
        for s in [
            "CREATE DEFINER=`root`@`localhost` PROCEDURE p() BEGIN SELECT 1; SELECT 2; END; select 3",
            "CREATE DEFINER = CURRENT_USER TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN SET NEW.a = 1; SET NEW.b = 2; END; select 3",
            "create definer=root@localhost event e on schedule every 1 day do begin delete from l where a = 1; select 1; end; select 3",
            "CREATE OR REPLACE FUNCTION f() RETURNS INT BEGIN DECLARE x INT; RETURN 1; END; select 3",
            "create temp trigger tr after insert on t begin select 1; select 2; end; select 3",
            "CREATE AGGREGATE FUNCTION f(x INT) RETURNS INT BEGIN DECLARE y INT; RETURN y; END; select 3",
            "ALTER EVENT e DO BEGIN SELECT 1; SELECT 2; END; select 3",
        ] {
            assert_eq!(texts(s, m).len(), 2, "{s}");
        }
    }

    #[test]
    fn dialect_presets_for_editor_hints() {
        assert_eq!(ScriptDialect::for_hint("postgres"), ScriptDialect::postgres());
        assert_eq!(ScriptDialect::for_hint("mysql"), ScriptDialect::mysql());
        assert_eq!(ScriptDialect::for_hint("mssql"), ScriptDialect::tsql());
        assert_eq!(ScriptDialect::for_hint("sybase"), ScriptDialect::tsql());
        assert_eq!(ScriptDialect::for_hint("oracle"), ScriptDialect::oracle());
        assert_eq!(ScriptDialect::for_hint("db2"), ScriptDialect::db2());
        assert_eq!(ScriptDialect::for_hint("standard"), ScriptDialect::generic());
        assert_eq!(ScriptDialect::for_hint(""), ScriptDialect::generic());
        // TDengine: "…" is a string, `…` a name.
        let td = ScriptDialect::for_hint("tdengine");
        assert!(td.backslash_escapes && td.backtick_idents);
        let toks = name_tokens("SELECT `a`, \"b\" FROM t", &td);
        assert!(toks.iter().any(|t| t.kind == TokenKind::String && t.text.contains('b')));
        assert!(toks.iter().any(|t| t.kind == TokenKind::Name && t.text == "a"));
    }

    #[test]
    fn unknown_wire_values_fall_back() {
        assert_eq!(serde_json::from_str::<ScriptMode>("\"parallel\"").unwrap(), ScriptMode::Whole);
        assert_eq!(serde_json::from_str::<ScriptMode>("\"batches\"").unwrap(), ScriptMode::Batches);
        let d: ScriptDialect = serde_json::from_str(r#"{"batch":"semicolon_line","plsql_blocks":true,"new_flag":1}"#).unwrap();
        assert_eq!((d.batch, d.plsql_blocks, d.semicolons), (BatchLine::None, true, true));
        let d: ScriptDialect = serde_json::from_str(r#"{"batch":"go"}"#).unwrap();
        assert_eq!(d.batch, BatchLine::Go);
        let x: ScriptDefaults = serde_json::from_str(r#"{"continue_on_error":true}"#).unwrap();
        assert!(x.continue_on_error && !x.confirm_unsafe_dml);
    }

    #[test]
    fn big_blocks_split_in_linear_time() {
        // A ~400 KB package body: each `;` used to rescan the block so far.
        let mut body = String::from("CREATE OR REPLACE PACKAGE BODY p AS\n");
        while body.len() < 400_000 {
            body.push_str("  PROCEDURE x IS BEGIN UPDATE t SET a = CASE WHEN b = 1 THEN 2 END; DELETE FROM u; END;\n");
        }
        body.push_str("END p;\n/\nselect 1 from dual;");
        let started = std::time::Instant::now();
        let st = split_script(&body, &ScriptDialect::oracle());
        assert_eq!(st.len(), 2);
        assert!(unsafe_statements(&body, &ScriptDialect::oracle()).is_empty());
        // Same shape as a MySQL routine.
        let mut r = String::from("create procedure p() begin\n");
        while r.len() < 400_000 {
            r.push_str("  if a then update t set a = 1; end if; case b when 1 then select 1; end case;\n");
        }
        r.push_str("end;\ndelete from t;");
        assert_eq!(split_script(&r, &ScriptDialect::mysql()).len(), 2);
        // Generous for a debug build; the quadratic version took minutes.
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    }
}

/// What [`name_tokens`] tells apart in a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A word or a quoted identifier (unquoted in `text`).
    Name,
    /// A string, dollar or q-quote, as written.
    String,
    /// Any other single character.
    Punct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameToken<'a> {
    pub kind: TokenKind,
    pub text: &'a str,
    /// Byte offset in the body.
    pub start: usize,
    /// Where it ends, quotes included (`start..end` is the token as written).
    pub end: usize,
}

/// The names, strings and punctuation of a body, comments and spaces left
/// out: what the dependency scan searches. `"…"` is a name except where the
/// dialect takes it as a string (MySQL, with backslash escapes).
pub fn name_tokens<'a>(text: &'a str, d: &ScriptDialect) -> Vec<NameToken<'a>> {
    let sc = Scanner { s: text, b: text.as_bytes(), d: *d };
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let (tok, end) = sc.token(i);
        let raw = &text[i..end];
        let kind = match tok {
            Tok::Word => Some(TokenKind::Name),
            Tok::Punct => Some(TokenKind::Punct),
            Tok::Quoted => Some(match raw.as_bytes()[0] {
                b'`' | b'[' => TokenKind::Name,
                b'"' if !d.backslash_escapes || d.dquote_idents => TokenKind::Name,
                _ => TokenKind::String,
            }),
            Tok::Space | Tok::LineComment | Tok::BlockComment => None,
        };
        if let Some(kind) = kind {
            let text = if kind == TokenKind::Name && raw.len() >= 2 && matches!(raw.as_bytes()[0], b'`' | b'[' | b'"') { &raw[1..raw.len() - 1] } else { raw };
            out.push(NameToken { kind, text, start: i, end });
        }
        i = end.max(i + 1);
    }
    out
}

/// [`name_tokens`], also inside PostgreSQL routine bodies: the `AS $tag$
/// … $tag$` of a `CREATE … FUNCTION|PROCEDURE` whose `LANGUAGE` is `sql` or
/// `plpgsql` is code, read with its offsets kept. Strings inside the body
/// (`EXECUTE '…'`, a `$q$` in `format()`) stay strings. What the dependency
/// scan and the rename search; other dollar blocks stay strings.
pub fn code_tokens<'a>(text: &'a str, d: &ScriptDialect) -> Vec<NameToken<'a>> {
    let toks = name_tokens(text, d);
    if !d.dollar_quotes {
        return toks;
    }
    let word = |t: &NameToken<'_>, w: &str| t.kind == TokenKind::Name && t.text.eq_ignore_ascii_case(w) && !matches!(text.as_bytes()[t.start], b'"');
    let creates: Vec<usize> = (0..toks.len()).filter(|&i| word(&toks[i], "create")).collect();
    let mut out = Vec::with_capacity(toks.len());
    for (i, t) in toks.iter().enumerate() {
        let body = t.kind == TokenKind::String && t.text.starts_with('$') && i > 0 && word(&toks[i - 1], "as") && {
            // The CREATE it belongs to, and where the next one starts.
            let from = creates.iter().rev().find(|&&c| c < i).copied();
            let to = creates.iter().find(|&&c| c > i).copied().unwrap_or(toks.len());
            from.is_some_and(|c| {
                let head = &toks[c..i];
                let routine = head.iter().take(6).any(|h| word(h, "function") || word(h, "procedure"));
                let language = toks[c..to].windows(2).find(|w| word(&w[0], "language")).map(|w| w[1].text.trim_matches('\'').to_ascii_lowercase());
                routine && matches!(language.as_deref(), Some("sql" | "plpgsql"))
            })
        };
        if !body {
            out.push(t.clone());
            continue;
        }
        let tag_len = t.text[1..].find('$').map_or(t.text.len(), |p| p + 2);
        let inner_end = if t.text.len() >= 2 * tag_len && t.text.ends_with(&t.text[..tag_len]) { t.text.len() - tag_len } else { t.text.len() };
        let base = t.start + tag_len;
        for n in name_tokens(&text[base..t.start + inner_end], d) {
            out.push(NameToken { start: n.start + base, end: n.end + base, ..n });
        }
    }
    out
}

#[cfg(test)]
mod dquote_ident_tests {
    use super::*;

    #[test]
    fn double_quoted_identifiers_ignore_backslashes_where_the_dialect_says() {
        // Snowflake: '…' takes backslash escapes, "…" only doubles `""`.
        let sf = ScriptDialect { backslash_escapes: true, dquote_idents: true, ..ScriptDialect::generic() };
        let parts = split_script("SELECT \"a\\\"; DELETE FROM t; SELECT 1", &sf);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(name_tokens("select \"x\\\" from t", &sf).iter().any(|t| t.kind == TokenKind::Name && t.text == "x\\"));
        // MySQL-like: "…" is a string with backslash escapes, as before.
        let my = ScriptDialect { backslash_escapes: true, ..ScriptDialect::generic() };
        assert_eq!(split_script("SELECT \"a\\\"; b\"; SELECT 1", &my).len(), 2);
    }
}

#[cfg(test)]
mod code_token_tests {
    use super::*;

    #[test]
    fn routine_bodies_are_code_and_their_strings_stay_strings() {
        let pg = ScriptDialect::postgres();
        let body = "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN EXECUTE 'select 1 from clientes'; RETURN (SELECT count(*) FROM clientes); END $$";
        let toks = code_tokens(body, &pg);
        let names: Vec<&str> = toks.iter().filter(|t| t.kind == TokenKind::Name).map(|t| t.text).collect();
        assert!(names.contains(&"clientes"));
        assert!(toks.iter().any(|t| t.kind == TokenKind::String && t.text.contains("from clientes")));
        // Offsets point into the whole text.
        for t in &toks {
            assert!(body[t.start..t.end].contains(t.text), "{t:?}");
        }
        // Not a routine body: a plain dollar string, or another language.
        assert!(!code_tokens("SELECT $$ from clientes $$", &pg).iter().any(|t| t.kind == TokenKind::Name && t.text == "clientes"));
        let js = "CREATE FUNCTION f() RETURNS int LANGUAGE plv8 AS $$ return clientes $$";
        assert!(!code_tokens(js, &pg).iter().any(|t| t.kind == TokenKind::Name && t.text == "clientes"));
    }
}
