//! "Renombrar…": rename an object, a column, an index, a constraint or a
//! schema, and rewrite the code that names it in one reviewed script.
//!
//! The driver writes the rename itself ([`crate::Driver::rename_script`]) and
//! says what its engine renames and how ([`RenameSpec`]). Finding the
//! dependents is "Ver dependencias" ([`crate::dependencies`]); rewriting
//! their text is [`rewrite_references`], next to it so both classify a name
//! the same way. The app only puts the pieces together.
//!
//! The rewrite is conservative: comments are never touched, strings never
//! rewritten (dynamic SQL), and anything it can't be sure of goes to
//! [`Rewrite::unresolved`] for the user to look at instead of being guessed.

use crate::dependencies::{eq, names_word, DependencyTarget, CONSTRAINT, SCHEMA};
use crate::kinds;
use crate::model::ObjectRef;
use crate::schema::TableSchema;
use crate::sql::{code_tokens, quote_ident, Quote, ScriptDialect, TokenKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What is renamed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum RenameTarget {
    /// A table, view, routine, trigger, sequence, collection, key…
    Object {
        object: ObjectRef,
        /// The owner of a dependent object: a trigger's table.
        #[serde(default)]
        parent: Option<String>,
    },
    Column { table: ObjectRef, column: String },
    Index { table: ObjectRef, index: String },
    Constraint { table: ObjectRef, constraint: String },
    Schema {
        #[serde(default)]
        database: Option<String>,
        schema: String,
    },
}

impl RenameTarget {
    /// The name that changes.
    pub fn old_name(&self) -> &str {
        match self {
            RenameTarget::Object { object, .. } => &object.name,
            RenameTarget::Column { column, .. } => column,
            RenameTarget::Index { index, .. } => index,
            RenameTarget::Constraint { constraint, .. } => constraint,
            RenameTarget::Schema { schema, .. } => schema,
        }
    }

    /// The table of a column, index or constraint.
    pub fn table(&self) -> Option<&ObjectRef> {
        match self {
            RenameTarget::Column { table, .. } | RenameTarget::Index { table, .. } | RenameTarget::Constraint { table, .. } => Some(table),
            _ => None,
        }
    }

    /// What "Ver dependencias" looks for: an index or a constraint as an
    /// object of its kind (T-SQL `WITH (INDEX(ix))`, `ALTER INDEX` inside a
    /// procedure…), a schema as a qualifier.
    pub fn dependency_target(&self) -> DependencyTarget {
        let named = |kind: &str, schema: Option<String>, name: &str| ObjectRef { kind: kind.into(), schema, name: name.into() };
        match self {
            RenameTarget::Object { object, .. } => DependencyTarget { object: object.clone(), column: None },
            RenameTarget::Column { table, column } => DependencyTarget { object: table.clone(), column: Some(column.clone()) },
            RenameTarget::Index { table, index } => DependencyTarget { object: named(kinds::INDEX, table.schema.clone(), index), column: None },
            RenameTarget::Constraint { table, constraint } => DependencyTarget { object: named(CONSTRAINT, table.schema.clone(), constraint), column: None },
            RenameTarget::Schema { schema, .. } => DependencyTarget { object: named(SCHEMA, None, schema), column: None },
        }
    }

    /// What [`rewrite_references`] looks for in a dependent.
    pub fn rewrite_target(&self) -> RewriteTarget {
        let t = self.dependency_target();
        match self {
            RenameTarget::Column { table, column } => RewriteTarget::Column { table: table.clone(), column: column.clone() },
            RenameTarget::Schema { schema, .. } => RewriteTarget::Schema { schema: schema.clone() },
            _ => RewriteTarget::Object { object: t.object },
        }
    }
}

/// A rename to script ([`crate::Driver::rename_script`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameRequest {
    pub target: RenameTarget,
    /// The new name exactly as it will be stored (case included): the
    /// driver quotes it where the engine needs it ([`quote_new`]).
    pub new_name: String,
    /// The table of a column, index or constraint, as `database_schema`
    /// reads it (MySQL's `CHANGE COLUMN` needs the whole column; Cassandra
    /// and ClickHouse refuse key columns).
    #[serde(default)]
    pub table: Option<TableSchema>,
    /// The object's own definition: routines, views and triggers on engines
    /// with no `RENAME` (dropped and created with a new header), PostgreSQL
    /// function signatures.
    #[serde(default)]
    pub definition: Option<String>,
}

/// How a rewritten dependent is put back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceStyle {
    /// Dropped before the rename and created after it (its grants go).
    #[default]
    DropCreate,
    /// `CREATE OR REPLACE` (keeps the grants).
    CreateOrReplace,
    /// `CREATE OR ALTER` (T-SQL; keeps permissions and the object id).
    CreateOrAlter,
}

/// How dependents name the target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceStyle {
    /// SQL text: names, qualifiers, quoted identifiers.
    #[default]
    Sql,
    /// Aggregation pipelines (MongoDB views): `viewOn`, `$lookup.from`,
    /// `$unionWith.coll`, `$out`, `$merge`.
    Pipeline,
    /// Dependents are listed, never rewritten.
    None,
}

/// How the engine folds unquoted names: what makes a new name need quotes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fold {
    /// PostgreSQL: unquoted names are lower case.
    Lower,
    /// Oracle, DB2, Firebird: unquoted names are upper case.
    Upper,
    /// Case is kept or ignored (SQL Server, MySQL, SQLite…).
    #[default]
    None,
}

impl Fold {
    fn apply(self, name: &str) -> String {
        match self {
            Fold::Lower => name.to_lowercase(),
            Fold::Upper => name.to_uppercase(),
            Fold::None => name.to_string(),
        }
    }
}

/// What a driver renames ([`crate::Driver::rename_spec`]); `None` there:
/// the explorer doesn't offer "Renombrar…".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RenameSpec {
    /// Object kinds it renames (`table`, `view`, `procedure`…).
    pub kinds: Vec<String>,
    pub columns: bool,
    pub indexes: bool,
    pub constraints: bool,
    pub schemas: bool,
    /// Dependent kinds the engine updates by itself (PostgreSQL views and
    /// triggers, SQLite views and triggers): listed, never rewritten.
    pub tracked: Vec<String>,
    pub replace: ReplaceStyle,
    pub references: ReferenceStyle,
    pub fold: Fold,
    /// Its DDL can run inside one transaction (PostgreSQL, SQL Server,
    /// SQLite): the app runs the script atomically.
    pub transactional: bool,
    /// What the dialog tells the user first (Spanish).
    pub note: Option<String>,
    /// `replace` for some kinds of dependent only (MySQL: views with
    /// `CREATE OR REPLACE`, routines and triggers dropped and created).
    pub replace_kinds: BTreeMap<String, ReplaceStyle>,
    /// Kinds that may keep rows of their own (a ClickHouse materialized
    /// view without `TO`): put back, they lose them, so a rewritten one
    /// starts unselected unless it writes `TO` another table
    /// ([`writes_to_table`]).
    pub holds_rows: Vec<String>,
    /// A statement the app runs after the whole script, rewritten
    /// dependents included (Oracle recompiles the schema, so what depends
    /// on them is valid again). `{schema}` stands for the target's schema
    /// as a string literal; without one it isn't run.
    pub epilogue: Option<String>,
}

impl RenameSpec {
    /// Whether it renames that target.
    pub fn allows(&self, target: &RenameTarget) -> bool {
        match target {
            RenameTarget::Object { object, .. } => self.kinds.contains(&object.kind),
            RenameTarget::Column { .. } => self.columns,
            RenameTarget::Index { .. } => self.indexes,
            RenameTarget::Constraint { .. } => self.constraints,
            RenameTarget::Schema { .. } => self.schemas,
        }
    }

    /// How a rewritten dependent of that kind is put back.
    pub fn replace_for(&self, kind: &str) -> ReplaceStyle {
        self.replace_kinds.get(kind).copied().unwrap_or(self.replace)
    }

    /// [`RenameSpec::epilogue`] for that target.
    pub fn epilogue_for(&self, target: &RenameTarget) -> Option<String> {
        let e = self.epilogue.as_deref()?;
        let schema = match target {
            RenameTarget::Object { object, .. } => object.schema(),
            RenameTarget::Schema { .. } => None,
            _ => target.table().and_then(|t| t.schema()),
        };
        if !e.contains("{schema}") {
            return Some(e.to_string());
        }
        Some(e.replace("{schema}", &format!("'{}'", schema?.replace('\'', "''"))))
    }
}

/// What a dependent's text is searched for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum RewriteTarget {
    /// An object (an index or constraint as an object of that kind).
    Object { object: ObjectRef },
    Column { table: ObjectRef, column: String },
    Schema { schema: String },
}

/// About the dependent being rewritten.
#[derive(Debug, Clone, Default)]
pub struct RewriteOptions {
    /// Its schema: an unqualified name in an object of another schema
    /// depends on the search path, and is left to the user.
    pub dependent_schema: Option<String>,
    /// It's a view and its output columns stay as they were: a renamed
    /// column in its select list becomes `nuevo AS viejo`.
    pub keep_view_columns: bool,
    /// The database, on engines where it is the schema (ClickHouse writes
    /// `db.t` in every stored definition): a name qualified with it is the
    /// target's when the target carries no schema.
    pub database: Option<String>,
    /// It's a routine a trigger on the target's table runs (a PostgreSQL
    /// trigger function): `NEW` and `OLD` are rows of that table.
    pub row_table: bool,
}

/// One rewritten line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edit {
    /// 1-based.
    pub line: u32,
    pub before: String,
    pub after: String,
}

/// Why a mention wasn't rewritten.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    /// Inside a string (dynamic SQL).
    InString,
    /// Qualified with something that may not be the target's schema.
    Qualified,
    /// Unqualified, in an object of another schema (search path).
    OtherSchema,
    /// Unquoted, while the target's name only matches quoted.
    Case,
    /// Followed by `(`: a function with the same name, maybe.
    MaybeFunction,
    /// A bare column in a statement that reads several relations.
    AmbiguousColumn,
    /// The schema's name is also an alias in the body.
    AliasNamedLikeSchema,
}

/// A mention left to the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Unresolved {
    pub line: u32,
    /// The line, trimmed.
    pub text: String,
    pub reason: UnresolvedReason,
}

/// A dependent's text with the target's new name.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Rewrite {
    pub text: String,
    pub edits: Vec<Edit>,
    pub unresolved: Vec<Unresolved>,
}

/// Words a new name can't be written bare as.
const RESERVED: &[&str] = &[
    "select", "from", "where", "table", "view", "order", "group", "by", "user", "index", "key", "primary", "foreign", "check", "default",
    "column", "constraint", "create", "drop", "alter", "insert", "update", "delete", "into", "values", "and", "or", "not", "null", "as",
    "on", "join", "union", "all", "distinct", "case", "when", "then", "else", "end", "begin", "grant", "to", "with", "having", "limit",
    "procedure", "function", "trigger", "schema", "database", "in", "is", "like", "between", "exists", "references", "unique", "set",
];

/// The new name may be written bare: a plain identifier, not reserved, and
/// already in the case the engine folds unquoted names to.
pub fn needs_quotes(name: &str, fold: Fold) -> bool {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    !plain || RESERVED.contains(&name.to_lowercase().as_str()) || fold.apply(name) != name
}

/// How the dialect quotes a name it writes.
fn preferred_quote(d: &ScriptDialect) -> Quote {
    if d.bracket_idents {
        Quote::Bracket
    } else if d.backtick_idents && d.backslash_escapes {
        Quote::Backtick
    } else {
        Quote::Double
    }
}

/// The new name as written in the dialect: quoted when the old one was, or
/// when it needs it ([`needs_quotes`]). Drivers quote the new name the same
/// way in their rename, so the rewritten code names what the rename made.
pub fn quote_new(name: &str, dialect: &ScriptDialect, fold: Fold, was_quoted: bool) -> String {
    if was_quoted || needs_quotes(name, fold) {
        quote_ident(preferred_quote(dialect), name)
    } else {
        name.to_string()
    }
}

/// A token of the body, with what the rewrite needs to know about it.
struct Tok<'a> {
    kind: TokenKind,
    text: &'a str,
    start: usize,
    end: usize,
    /// The quote it was written with (`"`, `[`, `` ` ``), for names.
    quote: Option<u8>,
    depth: u32,
    /// The `(` it's inside of.
    opener: Option<usize>,
}

impl Tok<'_> {
    /// The name as stored (doubled quotes inside undone).
    fn value(&self) -> String {
        match self.quote {
            Some(q) => {
                let close = if q == b'[' { ']' } else { q as char };
                self.text.replace(&format!("{close}{close}"), &close.to_string())
            }
            None => self.text.to_string(),
        }
    }

    /// An unquoted word, any case.
    fn word(&self, w: &str) -> bool {
        self.kind == TokenKind::Name && self.quote.is_none() && self.text.eq_ignore_ascii_case(w)
    }

    fn any(&self, ws: &[&str]) -> bool {
        ws.iter().any(|w| self.word(w))
    }

    fn punct(&self, c: &str) -> bool {
        self.kind == TokenKind::Punct && self.text == c
    }

    fn is_name(&self) -> bool {
        self.kind == TokenKind::Name
    }
}

fn tokens<'a>(body: &'a str, d: &ScriptDialect) -> Vec<Tok<'a>> {
    let mut out = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    for t in code_tokens(body, d) {
        let first = body.as_bytes()[t.start];
        let quote = (t.kind == TokenKind::Name && matches!(first, b'"' | b'[' | b'`')).then_some(first);
        if t.kind == TokenKind::Punct && t.text == ")" {
            open.pop();
        }
        out.push(Tok { kind: t.kind, text: t.text, start: t.start, end: t.end, quote, depth: open.len() as u32, opener: open.last().copied() });
        if t.kind == TokenKind::Punct && t.text == "(" {
            open.push(out.len() - 1);
        }
    }
    out
}

#[derive(PartialEq)]
enum Hit {
    No,
    Yes,
    /// Unquoted, while the stored name isn't in the folded case.
    Case,
}

/// Whether the name token `t` names `name`: unquoted any case, quoted
/// exactly (any case in T-SQL and MySQL, which compare names that way).
fn hit(t: &Tok<'_>, name: &str, d: &ScriptDialect, fold: Fold) -> Hit {
    if !t.is_name() {
        return Hit::No;
    }
    let v = t.value();
    if t.quote.is_some() && !(d.bracket_idents || d.backslash_escapes) {
        return if v == name { Hit::Yes } else { Hit::No };
    }
    if !eq(&v, name) {
        return Hit::No;
    }
    if t.quote.is_none() && fold.apply(name) != name {
        return Hit::Case;
    }
    Hit::Yes
}

/// The new name in the old token's quotes (or as the dialect needs it).
fn replacement(t: &Tok<'_>, new: &str, d: &ScriptDialect, fold: Fold) -> String {
    match t.quote {
        Some(b'[') => quote_ident(Quote::Bracket, new),
        Some(b'`') => quote_ident(Quote::Backtick, new),
        Some(_) => quote_ident(Quote::Double, new),
        None => quote_new(new, d, fold, false),
    }
}

/// `x.` before the name at `i`: the index of `x`.
fn qualifier(toks: &[Tok<'_>], i: usize) -> Option<usize> {
    (i >= 2 && toks[i - 1].punct(".") && toks[i - 2].is_name()).then(|| i - 2)
}

fn dotted_after(toks: &[Tok<'_>], i: usize) -> bool {
    toks.get(i + 1).is_some_and(|t| t.punct(".")) && toks.get(i + 2).is_some_and(|t| t.is_name())
}

/// The first `CREATE … <kind> [IF NOT EXISTS] name` header: the kind word
/// and the indexes of the name's first and last parts.
fn header(toks: &[Tok<'_>]) -> Option<(String, usize, usize)> {
    const KINDS: &[&str] = &[
        "view", "procedure", "proc", "function", "trigger", "package", "type", "sequence", "synonym", "table", "index", "event", "macro",
        "dictionary", "stream", "task", "sink", "alias",
    ];
    const SKIP: &[&str] = &[
        "or", "replace", "alter", "temp", "temporary", "materialized", "recursive", "editionable", "noneditionable", "force", "noforce",
        "constraint", "unique", "clustered", "nonclustered", "global", "local", "secure", "transient", "volatile", "external", "aggregate",
        "public", "body", "if", "not", "exists", "live", "unlogged", "definer", "invoker",
    ];
    let c = toks.iter().position(|t| t.word("create"))?;
    let mut i = c + 1;
    let mut kind: Option<String> = None;
    while i < toks.len() {
        let t = &toks[i];
        if t.any(&["algorithm"]) && toks.get(i + 1).is_some_and(|x| x.punct("=")) {
            i += 3;
        } else if t.word("definer") && toks.get(i + 1).is_some_and(|x| x.punct("=")) {
            i += 3;
            while toks.get(i).is_some_and(|x| x.punct("@")) {
                i += 2;
            }
            if toks.get(i).is_some_and(|x| x.punct("(")) && toks.get(i + 1).is_some_and(|x| x.punct(")")) {
                i += 2;
            }
        } else if t.word("sql") && toks.get(i + 1).is_some_and(|x| x.word("security")) {
            i += 3;
        } else if t.quote.is_none() && t.any(KINDS) && kind.as_deref().is_none_or(|k| k == "package" || k == "type") && !(kind.is_some() && !t.word("body")) {
            kind = Some(t.text.to_ascii_lowercase());
            i += 1;
        } else if t.quote.is_none() && t.any(SKIP) {
            i += 1;
        } else if t.is_name() && kind.is_some() {
            let mut last = i;
            while dotted_after(toks, last) {
                last += 2;
            }
            return kind.map(|k| (k, i, last));
        } else {
            return None;
        }
    }
    None
}

/// `definition` with the object's name (the last part of the name in its
/// `CREATE …` header) changed to `new_name`, its schema kept: for engines
/// that rename a routine, view or trigger by creating it again. `None` when
/// the header can't be read.
pub fn rename_header(definition: &str, dialect: &ScriptDialect, fold: Fold, new_name: &str) -> Option<String> {
    let toks = tokens(definition, dialect);
    let (_, _, last) = header(&toks)?;
    let t = &toks[last];
    Some(format!("{}{}{}", &definition[..t.start], replacement(t, new_name, dialect, fold), &definition[t.end..]))
}

/// `definition` with its `CREATE [OR REPLACE | OR ALTER]` made the
/// statement `style` puts a dependent back with. `DropCreate` keeps it.
pub fn with_create_style(definition: &str, dialect: &ScriptDialect, style: ReplaceStyle) -> String {
    let lead = match style {
        ReplaceStyle::DropCreate => return definition.to_string(),
        ReplaceStyle::CreateOrReplace => "CREATE OR REPLACE",
        ReplaceStyle::CreateOrAlter => "CREATE OR ALTER",
    };
    let toks = tokens(definition, dialect);
    let Some(c) = toks.iter().position(|t| t.word("create")) else { return definition.to_string() };
    let end = if toks.get(c + 1).is_some_and(|t| t.word("or")) && toks.get(c + 2).is_some_and(|t| t.any(&["replace", "alter"])) { toks[c + 2].end } else { toks[c].end };
    // The engine's own spacing (`CREATE      VIEW`) doesn't pile up.
    format!("{}{lead} {}", &definition[..toks[c].start], definition[end..].trim_start())
}

/// The definition is a view that writes `TO` another table (a ClickHouse
/// materialized view with `TO`): its rows live there, not in the view.
pub fn writes_to_table(definition: &str, dialect: &ScriptDialect) -> bool {
    let toks = tokens(definition, dialect);
    header(&toks).is_some_and(|(_, _, last)| toks.get(last + 1).is_some_and(|t| t.word("to")))
}

/// The definition is a trigger on a table called `table` (any schema).
pub fn trigger_on(definition: &str, dialect: &ScriptDialect, table: &str) -> bool {
    let toks = tokens(definition, dialect);
    let Some(("trigger", last)) = header(&toks).as_ref().map(|(k, _, l)| (k.as_str(), *l)) else { return false };
    trigger_table(&toks, last).is_some_and(|mut n| {
        while dotted_after(&toks, n) {
            n += 2;
        }
        eq(&toks[n].value(), table)
    })
}

/// The body names `word` as code (not in a string or a comment).
pub fn names_in_code(body: &str, dialect: &ScriptDialect, word: &str) -> bool {
    tokens(body, dialect).iter().any(|t| t.is_name() && eq(&t.value(), word))
}

/// The routine a trigger runs (`EXECUTE FUNCTION | PROCEDURE [s.]f(…)`,
/// PostgreSQL and its family): its schema, if written, and its name.
pub fn trigger_routine(definition: &str, dialect: &ScriptDialect) -> Option<(Option<String>, String)> {
    let toks = tokens(definition, dialect);
    let k = (0..toks.len()).find(|&k| toks[k].word("execute") && toks.get(k + 1).is_some_and(|t| t.any(&["function", "procedure"])))?;
    let first = k + 2;
    toks.get(first).filter(|t| t.is_name())?;
    let mut last = first;
    while dotted_after(&toks, last) {
        last += 2;
    }
    toks.get(last + 1).filter(|t| t.punct("("))?;
    let schema = (last > first).then(|| toks[last - 2].value());
    Some((schema, toks[last].value()))
}

/// Collects the replacements and the mentions left out.
struct Writer<'a> {
    body: &'a str,
    subs: Vec<(usize, usize, String)>,
    unresolved: Vec<(usize, UnresolvedReason)>,
}

impl Writer<'_> {
    fn sub(&mut self, start: usize, end: usize, text: String) {
        self.subs.push((start, end, text));
    }

    fn unsure(&mut self, at: usize, reason: UnresolvedReason) {
        self.unresolved.push((at, reason));
    }

    fn finish(mut self) -> Rewrite {
        self.subs.sort_by_key(|s| (s.0, s.1));
        self.subs.dedup_by_key(|s| (s.0, s.1));
        let mut text = String::with_capacity(self.body.len() + 16 * self.subs.len());
        let mut at = 0;
        let mut lines: Vec<u32> = Vec::new();
        for (start, end, repl) in &self.subs {
            if *start < at {
                continue;
            }
            text.push_str(&self.body[at..*start]);
            text.push_str(repl);
            at = *end;
            lines.push(line_of(self.body, *start));
        }
        text.push_str(&self.body[at..]);
        lines.dedup();
        let before: Vec<&str> = self.body.split('\n').collect();
        let after: Vec<&str> = text.split('\n').collect();
        let cut = |s: &str| s.trim().chars().take(crate::dependencies::LINE_CHARS).collect::<String>();
        let edits = lines
            .into_iter()
            .map(|l| Edit { line: l, before: cut(before.get(l as usize - 1).unwrap_or(&"")), after: cut(after.get(l as usize - 1).unwrap_or(&"")) })
            .collect();
        let mut unresolved: Vec<Unresolved> = self
            .unresolved
            .iter()
            .map(|&(at, reason)| {
                let line = line_of(self.body, at);
                Unresolved { line, text: cut(before.get(line as usize - 1).unwrap_or(&"")), reason }
            })
            .collect();
        unresolved.sort_by_key(|u| (u.line, u.reason));
        unresolved.dedup_by(|a, b| a.line == b.line && a.reason == b.reason);
        Rewrite { text, edits, unresolved }
    }
}

/// A string's text without its dollar tags (`$q$…$q$`), whose `$` would
/// glue to the words next to them.
fn string_body(text: &str) -> &str {
    if !text.starts_with('$') {
        return text;
    }
    let tag = text[1..].find('$').map_or(text.len(), |p| p + 2);
    let inner = &text[tag.min(text.len())..];
    inner.strip_suffix(&text[..tag.min(text.len())]).unwrap_or(inner)
}

fn line_of(body: &str, at: usize) -> u32 {
    1 + body[..at].bytes().filter(|&b| b == b'\n').count() as u32
}

/// `body` (a dependent's definition) with the target's name changed to
/// `new_name` where it names it for sure, what changed line by line, and
/// the mentions left to the user. Comments and strings are never touched;
/// the dependent's own name in its header neither.
pub fn rewrite_references(
    body: &str,
    dialect: &ScriptDialect,
    target: &RewriteTarget,
    new_name: &str,
    spec: &RenameSpec,
    opts: &RewriteOptions,
) -> Rewrite {
    let toks = tokens(body, dialect);
    let mut w = Writer { body, subs: Vec::new(), unresolved: Vec::new() };
    match (spec.references, target) {
        (ReferenceStyle::None, _) => {}
        (ReferenceStyle::Pipeline, RewriteTarget::Object { object }) => pipeline(&mut w, &toks, &object.name, new_name),
        (ReferenceStyle::Pipeline, _) => {}
        (ReferenceStyle::Sql, RewriteTarget::Object { object }) => objects(&mut w, &toks, dialect, spec.fold, object, new_name, opts),
        (ReferenceStyle::Sql, RewriteTarget::Column { table, column }) => columns(&mut w, &toks, dialect, spec.fold, table, column, new_name, opts),
        (ReferenceStyle::Sql, RewriteTarget::Schema { schema }) => schemas(&mut w, &toks, dialect, spec.fold, schema, new_name),
    }
    w.finish()
}

/// Kinds a `name(` may also be a same-named function for.
const RELATION_KINDS: &[&str] = &[kinds::TABLE, kinds::VIEW, kinds::MATERIALIZED_VIEW, kinds::COLLECTION, "virtual_table"];

fn objects(w: &mut Writer<'_>, toks: &[Tok<'_>], d: &ScriptDialect, fold: Fold, object: &ObjectRef, new: &str, opts: &RewriteOptions) {
    let own = header(toks).map(|(_, _, last)| last);
    let relation = RELATION_KINDS.contains(&object.kind.as_str());
    for (i, t) in toks.iter().enumerate() {
        if t.kind == TokenKind::String {
            if names_word(string_body(t.text), &object.name) {
                w.unsure(t.start, UnresolvedReason::InString);
            }
            continue;
        }
        if Some(i) == own {
            continue;
        }
        let h = hit(t, &object.name, d, fold);
        if h == Hit::No {
            continue;
        }
        let q = qualifier(toks, i);
        match (q, object.schema()) {
            (Some(q), Some(s)) if !eq(&toks[q].value(), s) => continue,
            (Some(q), None) if opts.database.as_deref().is_some_and(|db| eq(&toks[q].value(), db)) => {}
            (Some(_), None) => {
                w.unsure(t.start, UnresolvedReason::Qualified);
                continue;
            }
            (None, Some(s)) if opts.dependent_schema.as_deref().is_some_and(|ds| !ds.is_empty() && !eq(ds, s)) => {
                w.unsure(t.start, UnresolvedReason::OtherSchema);
                continue;
            }
            _ => {}
        }
        if h == Hit::Case {
            w.unsure(t.start, UnresolvedReason::Case);
            continue;
        }
        let first = q.unwrap_or(i);
        if first > 0 && toks[first - 1].word("as") {
            continue; // an alias called like it
        }
        if relation && toks.get(i + 1).is_some_and(|n| n.punct("(")) && !(first > 0 && toks[first - 1].any(&["into", "references", "table", "on"])) {
            w.unsure(t.start, UnresolvedReason::MaybeFunction);
            continue;
        }
        w.sub(t.start, t.end, replacement(t, new, d, fold));
    }
}

fn schemas(w: &mut Writer<'_>, toks: &[Tok<'_>], d: &ScriptDialect, fold: Fold, schema: &str, new: &str) {
    let aliases: Vec<String> = segments(toks).into_iter().flat_map(|(a, b)| relations(toks, a, b)).filter_map(|r| r.alias).collect();
    let aliased = aliases.iter().any(|a| eq(a, schema));
    let dotted = format!("{}.", schema.to_lowercase());
    for (i, t) in toks.iter().enumerate() {
        if t.kind == TokenKind::String {
            let text = t.text.to_lowercase();
            if text.match_indices(&dotted).any(|(p, _)| !text[..p].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_')) {
                w.unsure(t.start, UnresolvedReason::InString);
            }
            continue;
        }
        if !dotted_after(toks, i) {
            continue;
        }
        match hit(t, schema, d, fold) {
            Hit::No => {}
            Hit::Case => w.unsure(t.start, UnresolvedReason::Case),
            Hit::Yes if aliased => w.unsure(t.start, UnresolvedReason::AliasNamedLikeSchema),
            Hit::Yes => w.sub(t.start, t.end, replacement(t, new, d, fold)),
        }
    }
}

/// A relation a statement reads or writes (`FROM`, `JOIN`, `UPDATE`,
/// `INSERT INTO`, `MERGE INTO`, `USING`, a trigger's `ON`).
struct Rel {
    /// The last part of its name; `None` for a subquery or a function.
    name: Option<usize>,
    alias: Option<String>,
    /// Where its alias is.
    alias_at: Option<usize>,
    /// The `(` of an `INSERT INTO t (cols)` column list.
    columns: Option<usize>,
}

/// Words that end a relation instead of aliasing it.
const NOT_ALIAS: &[&str] = &[
    "where", "join", "inner", "left", "right", "full", "cross", "outer", "on", "set", "group", "order", "having", "union", "except",
    "intersect", "minus", "limit", "offset", "fetch", "for", "values", "select", "using", "with", "natural", "lateral", "apply", "when",
    "then", "returning", "window", "qualify", "into", "from", "pivot", "unpivot", "tablesample", "go", "begin", "end", "if", "else",
    "option", "straight_join", "partition", "and", "or", "not", "use", "ignore", "force", "default", "output", "insert", "update",
    "delete", "merge", "declare", "return", "while", "exec", "execute", "as", "do", "loop", "after", "before", "instead", "each", "row",
    "referencing", "execute",
];

/// The statements of a body, as token ranges: split at `;` and at the
/// words that start one (T-SQL needs no `;`), outside parentheses.
fn segments(toks: &[Tok<'_>]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    for (k, t) in toks.iter().enumerate() {
        if t.depth > 0 || k == start {
            if t.punct(";") && t.depth == 0 {
                out.push((start, k));
                start = k + 1;
            }
            continue;
        }
        if t.punct(";") {
            out.push((start, k));
            start = k + 1;
            continue;
        }
        let prev = &toks[k - 1];
        let starts = if t.word("select") {
            !prev.any(&["union", "all", "except", "intersect", "minus", "distinct"])
        } else if t.any(&["insert", "update", "delete", "merge"]) {
            !prev.any(&["then", "for", "on", "or", "of", "after", "before", "instead", "not"]) && !prev.punct(",")
        } else {
            t.any(&["begin", "declare", "if", "while", "return"]) && !prev.any(&["end", "exists"])
        };
        if starts {
            out.push((start, k));
            start = k;
        }
    }
    if start < toks.len() {
        out.push((start, toks.len()));
    }
    out
}

/// The relations of the statement `toks[a..b]`.
fn relations(toks: &[Tok<'_>], a: usize, b: usize) -> Vec<Rel> {
    let mut out = Vec::new();
    let trigger = toks[a..b].iter().take(8).any(|t| t.word("trigger"));
    let mut trigger_on = false;
    let mut k = a;
    while k < b {
        let t = &toks[k];
        let from = t.word("from") && match t.opener {
            // In parentheses: a subquery's FROM, not EXTRACT(x FROM y).
            Some(o) => toks.get(o + 1).is_some_and(|n| n.any(&["select", "with"])),
            None => true,
        };
        let into = t.word("into") && toks[a..k].iter().rev().take(3).any(|p| p.any(&["insert", "merge", "replace"]));
        let on = t.word("on") && trigger && !trigger_on;
        let opens = from || into || on || t.any(&["join", "using"]) || (t.word("update") && k == a);
        if !opens {
            k += 1;
            continue;
        }
        trigger_on |= on;
        let mut j = k + 1;
        while let Some(n) = toks.get(j).filter(|_| j < b) {
            let mut rel = Rel { name: None, alias: None, alias_at: None, columns: None };
            if n.punct("(") {
                if t.word("using") {
                    break; // JOIN … USING (col)
                }
                // A subquery: skip to its `)`.
                j = (j + 1..b).find(|&m| toks[m].punct(")") && toks[m].opener.is_none_or(|o| o < j) && toks[m].depth == n.depth).map_or(b, |m| m + 1);
            } else if let Some(last) = table_call(toks, j).filter(|_| !into) {
                // `table(s)` (Proton): the stream read as a table.
                rel.name = Some(last);
                j = last + 2;
            } else if n.is_name() && !n.text.as_bytes()[0].is_ascii_digit() && !(n.quote.is_none() && n.any(NOT_ALIAS) && !dotted_after(toks, j)) {
                // A keyword before a dot is a qualifier (`default.t`).
                let mut last = j;
                while dotted_after(toks, last) {
                    last += 2;
                }
                j = last + 1;
                if toks.get(j).is_some_and(|x| x.punct("(")) && !into {
                    // A table function: its columns are its own.
                    j = (j + 1..b).find(|&m| toks[m].punct(")") && toks[m].depth == toks[last].depth).map_or(b, |m| m + 1);
                } else {
                    rel.name = Some(last);
                }
            } else {
                break;
            }
            // [AS] alias
            let mut x = j;
            if toks.get(x).is_some_and(|y| y.word("as")) && x < b {
                x += 1;
            }
            if let Some(y) = toks.get(x).filter(|y| x < b && y.is_name() && !(y.quote.is_none() && y.any(NOT_ALIAS))) {
                rel.alias = Some(y.value());
                rel.alias_at = Some(x);
                j = x + 1;
            }
            if into && toks.get(j).is_some_and(|y| y.punct("(")) {
                rel.columns = Some(j);
            }
            out.push(rel);
            if from && toks.get(j).is_some_and(|y| y.punct(",")) {
                j += 1;
                continue;
            }
            break;
        }
        k = j.max(k + 1);
    }
    out
}

/// `table(name)` at `j`: the last part of the name.
fn table_call(toks: &[Tok<'_>], j: usize) -> Option<usize> {
    if !(toks[j].word("table") && toks.get(j + 1).is_some_and(|t| t.punct("(")) && toks.get(j + 2).is_some_and(|t| t.is_name())) {
        return None;
    }
    let mut last = j + 2;
    while dotted_after(toks, last) {
        last += 2;
    }
    toks.get(last + 1).is_some_and(|t| t.punct(")")).then_some(last)
}

/// The table of a trigger whose name ends at `last`: after `ON` (most
/// engines) or `FOR` (Firebird; not `FOR EACH ROW`), before its body. The
/// index of the name's first part.
fn trigger_table(toks: &[Tok<'_>], last: usize) -> Option<usize> {
    for k in last + 1..toks.len() {
        let t = &toks[k];
        if t.depth > 0 {
            continue;
        }
        if t.any(&["as", "begin", "declare", "is", "set", "execute", "call"]) {
            return None;
        }
        let on = t.word("on") || (t.word("for") && !toks.get(k + 1).is_some_and(|n| n.any(&["each", "row", "statement"])));
        if on && toks.get(k + 1).is_some_and(|n| n.is_name()) {
            return Some(k + 1);
        }
    }
    None
}

/// `REFERENCING NEW [ROW] AS n OLD AS o` (Oracle, Db2…): the row aliases.
fn row_aliases(toks: &[Tok<'_>], last: usize) -> Vec<String> {
    let mut out = Vec::new();
    let Some(r) = (last + 1..toks.len()).find(|&k| toks[k].depth == 0 && toks[k].word("referencing")) else { return out };
    let mut k = r + 1;
    while toks.get(k).is_some_and(|t| t.any(&["new", "old", "new_table", "old_table", "parent"])) {
        k += 1;
        if toks.get(k).is_some_and(|t| t.any(&["row", "table"])) {
            k += 1;
        }
        if toks.get(k).is_some_and(|t| t.word("as")) {
            k += 1;
        }
        match toks.get(k).filter(|t| t.is_name()) {
            Some(t) => out.push(t.value()),
            None => break,
        }
        k += 1;
    }
    out
}

/// The pseudo-tables of a trigger's own table.
const ROW_TABLES: &[&str] = &["inserted", "deleted", "new", "old"];

#[allow(clippy::too_many_arguments)]
fn columns(w: &mut Writer<'_>, toks: &[Tok<'_>], d: &ScriptDialect, fold: Fold, table: &ObjectRef, column: &str, new: &str, opts: &RewriteOptions) {
    let names_table = |idx: usize| {
        hit(&toks[idx], &table.name, d, fold) != Hit::No
            && match (qualifier(toks, idx), table.schema()) {
                (Some(q), Some(s)) => eq(&toks[q].value(), s),
                _ => true,
            }
    };
    // A trigger on the table (or a routine one runs): NEW / OLD /
    // inserted / deleted, and the names REFERENCING gives them, are its rows.
    let head = header(toks);
    let trigger = head.as_ref().filter(|(kind, _, _)| kind == "trigger").map(|(_, _, last)| *last);
    let on_table = opts.row_table
        || trigger.and_then(|last| trigger_table(toks, last)).is_some_and(|mut n| {
            while dotted_after(toks, n) {
                n += 2;
            }
            names_table(n)
        });
    let aliases = trigger.map(|last| row_aliases(toks, last)).unwrap_or_default();
    let is_row = |t: &Tok<'_>| t.quote.is_none() && (t.any(ROW_TABLES) || aliases.iter().any(|a| eq(a, t.text)));
    let own = head.as_ref().map(|(_, _, last)| *last);
    let view_list = if opts.keep_view_columns { view_select_list(toks, head.as_ref()) } else { None };
    for (a, b) in segments(toks) {
        let rels = relations(toks, a, b);
        // `UPDATE c … FROM Clientes c`: `c` is the alias, not another table.
        let rels: Vec<&Rel> = rels
            .iter()
            .filter(|r| !(r.name.is_some_and(|n| qualifier(toks, n).is_none() && rels.iter().any(|o| o.alias.as_deref().is_some_and(|al| eq(al, &toks[n].value()))))))
            .collect();
        let is_target = |r: &Rel| r.name.is_some_and(|n| names_table(n) || (on_table && is_row(&toks[n])));
        let targets = rels.iter().filter(|r| is_target(r)).count();
        let skip: Vec<usize> = rels.iter().flat_map(|r| r.name.into_iter().chain(r.alias_at)).collect();
        let column_lists: Vec<usize> = rels.iter().filter(|r| is_target(r)).filter_map(|r| r.columns).collect();
        // The qualifier `q` names the table (or one of its rows).
        let resolves = |q: usize| -> Option<bool> {
            let v = toks[q].value();
            if let Some(r) = rels.iter().find(|r| r.alias.as_deref().is_some_and(|al| eq(al, &v))) {
                return Some(is_target(r));
            }
            if on_table && is_row(&toks[q]) {
                return Some(true);
            }
            if names_table(q) {
                return Some(true);
            }
            rels.iter().find(|r| r.name.is_some_and(|n| eq(&toks[n].value(), &v))).map(|r| is_target(r))
        };
        let mut strings = false;
        for i in a..b {
            let t = &toks[i];
            if t.kind == TokenKind::String {
                strings |= names_word(string_body(t.text), column);
                continue;
            }
            if Some(i) == own || skip.contains(&i) {
                continue;
            }
            let h = hit(t, column, d, fold);
            if h == Hit::No || dotted_after(toks, i) || toks.get(i + 1).is_some_and(|n| n.punct("(")) {
                continue;
            }
            let first = qualifier(toks, i).unwrap_or(i);
            if first > 0 && toks[first - 1].word("as") {
                continue; // an output alias called like it
            }
            let sure = match qualifier(toks, i) {
                Some(q) => match resolves(q) {
                    Some(true) => true,
                    _ => continue,
                },
                None if t.opener.is_some_and(|o| column_lists.contains(&o)) => true,
                None if targets == 0 => continue,
                None if targets == rels.len() => true,
                None => {
                    w.unsure(t.start, UnresolvedReason::AmbiguousColumn);
                    continue;
                }
            };
            if !sure {
                continue;
            }
            if h == Hit::Case {
                w.unsure(t.start, UnresolvedReason::Case);
                continue;
            }
            let mut repl = replacement(t, new, d, fold);
            if view_list.as_ref().is_some_and(|items| items.iter().any(|&(s, e)| first == s && i + 1 == e)) {
                // The view's output column keeps its name.
                repl = format!("{repl} AS {}", &w.body[t.start..t.end]);
            }
            w.sub(t.start, t.end, repl);
        }
        if strings && (targets > 0 || on_table) {
            for t in &toks[a..b] {
                if t.kind == TokenKind::String && names_word(string_body(t.text), column) {
                    w.unsure(t.start, UnresolvedReason::InString);
                }
            }
        }
    }
}

/// A view's top-level select list, as token ranges of its items: the ones
/// without an alias may need one when their column is renamed. `None` when
/// the view lists its column names itself (`CREATE VIEW v (a, b) AS`): the
/// output names come from the list, by position. A list with types
/// (ClickHouse stores `v (`a` UInt64, …)`) is a structure the select is
/// matched to by name, so the select keeps its names.
fn view_select_list(toks: &[Tok<'_>], head: Option<&(String, usize, usize)>) -> Option<Vec<(usize, usize)>> {
    let (kind, _, last) = head?;
    if !kind.ends_with("view") && kind != "view" {
        return None;
    }
    if toks.get(last + 1).is_some_and(|t| t.punct("(")) {
        let open = &toks[last + 1];
        let close = (last + 2..toks.len()).find(|&k| toks[k].punct(")") && toks[k].depth == open.depth)?;
        let names_only = toks[last + 2..close].split(|t| t.punct(",") && t.depth == open.depth + 1).all(|item| item.len() == 1 && item[0].is_name());
        if names_only {
            return None;
        }
    }
    let select = (last + 1..toks.len()).find(|&k| toks[k].depth == 0 && toks[k].word("select"))?;
    let mut k = select + 1;
    while toks.get(k).is_some_and(|t| t.any(&["distinct", "all"])) {
        k += 1;
    }
    if toks.get(k).is_some_and(|t| t.word("top")) {
        k += 2;
    }
    let mut items = Vec::new();
    let mut start = k;
    while k < toks.len() {
        let t = &toks[k];
        if t.depth == 0 && (t.punct(",") || t.any(&["from", "union", "except", "intersect", "minus", "into", "where"]) || t.punct(";")) {
            items.push((start, k));
            if !t.punct(",") {
                break;
            }
            start = k + 1;
        }
        k += 1;
    }
    if k == toks.len() {
        items.push((start, k));
    }
    // Only items that are a column alone (`c`, `t.c`, `s.t.c`).
    Some(items.into_iter().filter(|&(s, e)| e > s && (e - s) % 2 == 1 && (s..e).all(|k| if (k - s) % 2 == 0 { toks[k].is_name() } else { toks[k].punct(".") })).collect())
}

/// MongoDB pipelines: the collection names in `viewOn`, `from`
/// (`$lookup`, `$graphLookup`), `coll` (`$unionWith`), `into` / `$out` /
/// `$merge`, and `db.createView`'s source. A key is never a value.
fn pipeline(w: &mut Writer<'_>, toks: &[Tok<'_>], old: &str, new: &str) {
    const KEYS: &[&str] = &["viewOn", "from", "coll", "into", "$out", "$merge"];
    let value = |t: &Tok<'_>| -> Option<(u8, String)> {
        match (t.kind, t.quote) {
            (TokenKind::Name, Some(b'"')) => Some((b'"', t.value())),
            (TokenKind::String, _) if t.text.len() >= 2 => Some((t.text.as_bytes()[0], t.text[1..t.text.len() - 1].to_string())),
            _ => None,
        }
    };
    let quoted = |q: u8| {
        if q == b'"' {
            serde_json::to_string(new).unwrap_or_default()
        } else {
            format!("'{}'", new.replace('\\', "\\\\").replace('\'', "\\'"))
        }
    };
    let mut handled = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let key = match (t.kind, t.quote) {
            (TokenKind::Name, _) => Some(t.value()),
            (TokenKind::String, _) => value(t).map(|v| v.1),
            _ => None,
        };
        let is_key = key.as_deref().is_some_and(|k| KEYS.contains(&k)) && toks.get(i + 1).is_some_and(|n| n.punct(":"));
        let create_view = t.word("createView") && toks.get(i + 1).is_some_and(|n| n.punct("("));
        let at = if is_key {
            Some(i + 2)
        } else if create_view {
            // The second argument: after the first top-level comma.
            (i + 2..toks.len()).find(|&k| toks[k].punct(",") && toks[k].depth == toks[i + 2].depth).map(|k| k + 1)
        } else {
            None
        };
        if let Some(v) = at.and_then(|k| toks.get(k).map(|t| (k, t))) {
            if let Some((q, val)) = value(v.1) {
                handled.push(v.0);
                if val == old {
                    w.sub(v.1.start, v.1.end, quoted(q));
                }
            }
        }
    }
    for (i, t) in toks.iter().enumerate() {
        if handled.contains(&i) || toks.get(i + 1).is_some_and(|n| n.punct(":")) {
            continue;
        }
        if value(t).is_some_and(|(_, v)| v == old) {
            w.unsure(t.start, UnresolvedReason::InString);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(fold: Fold) -> RenameSpec {
        RenameSpec { fold, ..Default::default() }
    }

    fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: (!schema.is_empty()).then(|| schema.into()), name: name.into() }
    }

    fn table(schema: &str, name: &str) -> RewriteTarget {
        RewriteTarget::Object { object: obj(kinds::TABLE, schema, name) }
    }

    fn column(schema: &str, name: &str, col: &str) -> RewriteTarget {
        RewriteTarget::Column { table: obj(kinds::TABLE, schema, name), column: col.into() }
    }

    fn tsql() -> ScriptDialect {
        ScriptDialect::tsql()
    }

    fn rw(body: &str, d: &ScriptDialect, target: &RewriteTarget, new: &str) -> Rewrite {
        rewrite_references(body, d, target, new, &spec(Fold::None), &RewriteOptions::default())
    }

    fn view(body: &str, d: &ScriptDialect, target: &RewriteTarget, new: &str) -> Rewrite {
        rewrite_references(body, d, target, new, &spec(Fold::None), &RewriteOptions { keep_view_columns: true, ..Default::default() })
    }

    // 1
    #[test]
    fn comments_and_strings_are_not_touched() {
        let body = "-- reads Clientes\nSELECT 1 FROM dbo.Clientes /* Clientes */ WHERE x = 'Clientes'";
        let r = rw(body, &tsql(), &table("dbo", "Clientes"), "Nuevo");
        assert_eq!(r.text, "-- reads Clientes\nSELECT 1 FROM dbo.Nuevo /* Clientes */ WHERE x = 'Clientes'");
        assert_eq!(r.edits, vec![Edit { line: 2, before: "SELECT 1 FROM dbo.Clientes /* Clientes */ WHERE x = 'Clientes'".into(), after: "SELECT 1 FROM dbo.Nuevo /* Clientes */ WHERE x = 'Clientes'".into() }]);
        assert_eq!(r.unresolved.len(), 1);
        assert_eq!(r.unresolved[0].reason, UnresolvedReason::InString);
    }

    // 2
    #[test]
    fn other_schemas_quoting_and_case() {
        let r = rw("SELECT * FROM ventas.Clientes JOIN [dbo].[Clientes] ON 1 = 1", &tsql(), &table("dbo", "Clientes"), "Nuevo");
        assert_eq!(r.text, "SELECT * FROM ventas.Clientes JOIN [dbo].[Nuevo] ON 1 = 1");
        // PostgreSQL: quoted names are case-sensitive, unquoted ones fold.
        let pg = ScriptDialect::postgres();
        let p = spec(Fold::Lower);
        let r = rewrite_references("SELECT * FROM \"Clientes\", CLIENTES, \"clientes\"", &pg, &table("", "clientes"), "nuevo", &p, &RewriteOptions::default());
        assert_eq!(r.text, "SELECT * FROM \"Clientes\", nuevo, \"nuevo\"");
        // A mixed-case name only matches quoted: unquoted it's another object.
        let r = rewrite_references("SELECT * FROM \"Clientes\", clientes", &pg, &table("", "Clientes"), "Nuevo", &p, &RewriteOptions::default());
        assert_eq!(r.text, "SELECT * FROM \"Nuevo\", clientes");
        assert_eq!(r.unresolved[0].reason, UnresolvedReason::Case);
    }

    // 3
    #[test]
    fn whole_names_only() {
        let r = rw("SELECT * FROM ClientesViejos JOIN Clientes_hist ON 1=1 JOIN Clientes ON 1=1", &tsql(), &table("dbo", "Clientes"), "Nuevo");
        assert_eq!(r.text, "SELECT * FROM ClientesViejos JOIN Clientes_hist ON 1=1 JOIN Nuevo ON 1=1");
    }

    // 4
    #[test]
    fn columns_through_aliases() {
        let t = column("dbo", "Clientes", "Pepe");
        let r = rw("SELECT c.Pepe, p.Pepe FROM dbo.Clientes c JOIN Pedidos p ON p.id = c.id", &tsql(), &t, "Nuevo");
        assert_eq!(r.text, "SELECT c.Nuevo, p.Pepe FROM dbo.Clientes c JOIN Pedidos p ON p.id = c.id");
        assert!(r.unresolved.is_empty());
        let r = rw("SELECT Pepe FROM dbo.Clientes c JOIN Pedidos p ON p.id = c.id", &tsql(), &t, "Nuevo");
        assert_eq!(r.text, "SELECT Pepe FROM dbo.Clientes c JOIN Pedidos p ON p.id = c.id");
        assert_eq!(r.unresolved[0].reason, UnresolvedReason::AmbiguousColumn);
        let r = rw("UPDATE dbo.Clientes SET Pepe = 1 WHERE Pepe > 0;\nSELECT Pepe FROM Pedidos", &tsql(), &t, "Nuevo");
        assert_eq!(r.text, "UPDATE dbo.Clientes SET Nuevo = 1 WHERE Nuevo > 0;\nSELECT Pepe FROM Pedidos");
        // INSERT … SELECT: the column list is the table's.
        let r = rw("INSERT INTO Clientes (id, Pepe) SELECT id, Pepe FROM Otra", &tsql(), &t, "Nuevo");
        assert_eq!(r.text, "INSERT INTO Clientes (id, Nuevo) SELECT id, Pepe FROM Otra");
    }

    // 5
    #[test]
    fn an_alias_is_not_the_column() {
        let r = rw("SELECT pepe.total FROM dbo.Clientes pepe", &tsql(), &column("dbo", "Clientes", "Pepe"), "Nuevo");
        assert_eq!(r.text, "SELECT pepe.total FROM dbo.Clientes pepe");
        assert!(r.edits.is_empty() && r.unresolved.is_empty());
    }

    // 6
    #[test]
    fn view_columns_keep_their_names() {
        let t = column("dbo", "Clientes", "Pepe");
        let r = view("CREATE VIEW dbo.v AS\nSELECT Pepe, c.Pepe, Pepe AS x, Pepe + 1 AS y FROM dbo.Clientes c", &tsql(), &t, "Nuevo");
        assert_eq!(r.text, "CREATE VIEW dbo.v AS\nSELECT Nuevo AS Pepe, c.Nuevo AS Pepe, Nuevo AS x, Nuevo + 1 AS y FROM dbo.Clientes c");
        assert_eq!(r.edits.len(), 1);
        assert_eq!(r.edits[0].line, 2);
        // Off: the names change too.
        let r = rw("CREATE VIEW v AS SELECT Pepe FROM Clientes", &tsql(), &t, "Nuevo");
        assert_eq!(r.text, "CREATE VIEW v AS SELECT Nuevo FROM Clientes");
        // A view that names its columns keeps them anyway.
        let r = view("CREATE VIEW v (a) AS SELECT Pepe FROM Clientes", &tsql(), &t, "Nuevo");
        assert_eq!(r.text, "CREATE VIEW v (a) AS SELECT Nuevo FROM Clientes");
    }

    // 7
    #[test]
    fn plpgsql_bodies_are_rewritten_but_not_their_dynamic_sql() {
        let pg = ScriptDialect::postgres();
        let body = "CREATE OR REPLACE FUNCTION public.f() RETURNS bigint LANGUAGE plpgsql AS $$\nBEGIN\n  EXECUTE 'select count(*) from clientes';\n  RETURN (SELECT count(*) FROM public.clientes);\nEND $$";
        let r = rewrite_references(body, &pg, &table("public", "clientes"), "nuevo", &spec(Fold::Lower), &RewriteOptions::default());
        assert!(r.text.contains("FROM public.nuevo);"), "{}", r.text);
        assert!(r.text.contains("'select count(*) from clientes'"));
        assert_eq!(r.unresolved.len(), 1);
        assert_eq!(r.unresolved[0].line, 3);
        // A `$q$` inside format() isn't a body.
        let body = "CREATE FUNCTION g() RETURNS text LANGUAGE plpgsql AS $$ BEGIN RETURN format($q$select 1 from clientes$q$); END $$";
        let r = rewrite_references(body, &pg, &table("", "clientes"), "nuevo", &spec(Fold::Lower), &RewriteOptions::default());
        assert!(r.text.contains("$q$select 1 from clientes$q$"));
        assert_eq!(r.unresolved[0].reason, UnresolvedReason::InString);
    }

    // 8
    #[test]
    fn mysql_double_quotes_are_strings() {
        let my = ScriptDialect::mysql();
        let r = rw("SELECT \"clientes\", `clientes`.id FROM `Clientes`", &my, &table("", "clientes"), "nuevo");
        assert_eq!(r.text, "SELECT \"clientes\", `nuevo`.id FROM `nuevo`");
        assert_eq!(r.unresolved[0].reason, UnresolvedReason::InString);
    }

    // 9
    #[test]
    fn oracle_q_quotes_and_hints() {
        let ora = ScriptDialect::oracle();
        let body = "SELECT /*+ INDEX(c IX_CLIENTES) */ q'[ CLIENTES ]' FROM CLIENTES c";
        let r = rewrite_references(body, &ora, &table("", "CLIENTES"), "NUEVO", &spec(Fold::Upper), &RewriteOptions::default());
        assert_eq!(r.text, "SELECT /*+ INDEX(c IX_CLIENTES) */ q'[ CLIENTES ]' FROM NUEVO c");
        let ix = RewriteTarget::Object { object: obj(kinds::INDEX, "", "IX_CLIENTES") };
        let r = rewrite_references(body, &ora, &ix, "IX_NUEVO", &spec(Fold::Upper), &RewriteOptions::default());
        assert!(r.edits.is_empty());
    }

    // 10
    #[test]
    fn a_name_before_a_parenthesis_may_be_a_function() {
        let r = rw("SELECT Clientes(1); INSERT INTO Clientes (a, b) VALUES (1, 2)", &tsql(), &table("dbo", "Clientes"), "Nuevo");
        assert_eq!(r.text, "SELECT Clientes(1); INSERT INTO Nuevo (a, b) VALUES (1, 2)");
        assert_eq!(r.unresolved[0].reason, UnresolvedReason::MaybeFunction);
    }

    // 11
    #[test]
    fn unqualified_names_in_another_schema_are_left() {
        let opts = RewriteOptions { dependent_schema: Some("ventas".into()), ..Default::default() };
        let r = rewrite_references("SELECT * FROM Clientes JOIN dbo.Clientes x ON 1=1", &tsql(), &table("dbo", "Clientes"), "Nuevo", &spec(Fold::None), &opts);
        assert_eq!(r.text, "SELECT * FROM Clientes JOIN dbo.Nuevo x ON 1=1");
        assert_eq!(r.unresolved[0].reason, UnresolvedReason::OtherSchema);
    }

    // 12
    #[test]
    fn schema_renames_touch_qualifiers_only() {
        let s = RewriteTarget::Schema { schema: "ventas".into() };
        let r = rw("SELECT ventas, x.ventas FROM ventas.t x JOIN [ventas].u ON 1=1", &tsql(), &s, "comercial");
        assert_eq!(r.text, "SELECT ventas, x.ventas FROM comercial.t x JOIN [comercial].u ON 1=1");
        // An alias called like the schema: left to the user.
        let r = rw("SELECT ventas.id FROM ventas.t ventas", &tsql(), &s, "comercial");
        assert!(r.edits.is_empty());
        assert!(r.unresolved.iter().all(|u| u.reason == UnresolvedReason::AliasNamedLikeSchema));
    }

    // 13
    #[test]
    fn mongodb_pipelines() {
        let p = RenameSpec { references: ReferenceStyle::Pipeline, ..Default::default() };
        let body = r#"db.createView("v", "clientes", [{"$lookup": {"from": "clientes", "localField": "a", "foreignField": "b", "as": "c"}}, {"$unionWith": {"coll": "clientes"}}, {"$project": {"clientes": 1}}])"#;
        let r = rewrite_references(body, &ScriptDialect::generic(), &RewriteTarget::Object { object: obj(kinds::COLLECTION, "", "clientes") }, "nuevos", &p, &RewriteOptions::default());
        assert_eq!(
            r.text,
            r#"db.createView("v", "nuevos", [{"$lookup": {"from": "nuevos", "localField": "a", "foreignField": "b", "as": "c"}}, {"$unionWith": {"coll": "nuevos"}}, {"$project": {"clientes": 1}}])"#
        );
        assert!(r.unresolved.is_empty());
        let r = rewrite_references(r#"{"viewOn": "clientes", "pipeline": [{"$match": {"t": "clientes"}}]}"#, &ScriptDialect::generic(), &RewriteTarget::Object { object: obj(kinds::COLLECTION, "", "clientes") }, "nuevos", &p, &RewriteOptions::default());
        assert!(r.text.starts_with(r#"{"viewOn": "nuevos""#));
        assert_eq!(r.unresolved.len(), 1);
    }

    // 14
    #[test]
    fn unicode_and_doubled_quotes_keep_offsets() {
        let r = rw("SELECT 'ñandú' FROM [a]]b] JOIN Clientes ON 1=1", &tsql(), &table("dbo", "a]b"), "año");
        assert_eq!(r.text, "SELECT 'ñandú' FROM [año] JOIN Clientes ON 1=1");
        let pg = ScriptDialect::postgres();
        let r = rewrite_references("SELECT 'ü' FROM \"a\"\"b\"", &pg, &table("", "a\"b"), "c\"d", &spec(Fold::Lower), &RewriteOptions::default());
        assert_eq!(r.text, "SELECT 'ü' FROM \"c\"\"d\"");
    }

    // 15
    #[test]
    fn headers() {
        let t = tsql();
        assert_eq!(rename_header("CREATE OR ALTER PROC [dbo].[p] AS SELECT 1", &t, Fold::None, "q").unwrap(), "CREATE OR ALTER PROC [dbo].[q] AS SELECT 1");
        let my = ScriptDialect::mysql();
        assert_eq!(
            rename_header("CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select 1", &my, Fold::None, "w").unwrap(),
            "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `w` AS select 1"
        );
        let ora = ScriptDialect::oracle();
        assert_eq!(rename_header("CREATE OR REPLACE PACKAGE BODY APP.PKG AS END;", &ora, Fold::Upper, "PKG2").unwrap(), "CREATE OR REPLACE PACKAGE BODY APP.PKG2 AS END;");
        assert_eq!(rename_header("CREATE TRIGGER tg ON t AFTER INSERT AS SELECT 1", &t, Fold::None, "tg2").unwrap(), "CREATE TRIGGER tg2 ON t AFTER INSERT AS SELECT 1");
        assert!(rename_header("SELECT 1", &t, Fold::None, "x").is_none());
        assert_eq!(with_create_style("CREATE VIEW v AS SELECT 1", &t, ReplaceStyle::CreateOrAlter), "CREATE OR ALTER VIEW v AS SELECT 1");
        assert_eq!(with_create_style("create or replace view v as select 1", &t, ReplaceStyle::CreateOrAlter), "CREATE OR ALTER view v as select 1");
        assert_eq!(with_create_style("CREATE VIEW v AS SELECT 1", &t, ReplaceStyle::CreateOrReplace), "CREATE OR REPLACE VIEW v AS SELECT 1");
        assert_eq!(with_create_style("CREATE VIEW v AS SELECT 1", &t, ReplaceStyle::DropCreate), "CREATE VIEW v AS SELECT 1");
    }

    // 16
    #[test]
    fn quoting_new_names() {
        let pg = ScriptDialect::postgres();
        assert_eq!(quote_new("nuevo", &pg, Fold::Lower, false), "nuevo");
        assert_eq!(quote_new("Nuevo", &pg, Fold::Lower, false), "\"Nuevo\"");
        assert_eq!(quote_new("NUEVO", &ScriptDialect::oracle(), Fold::Upper, false), "NUEVO");
        assert_eq!(quote_new("nuevo", &ScriptDialect::oracle(), Fold::Upper, false), "\"nuevo\"");
        assert_eq!(quote_new("Nuevo", &tsql(), Fold::None, false), "Nuevo");
        assert_eq!(quote_new("mi tabla", &tsql(), Fold::None, false), "[mi tabla]");
        assert_eq!(quote_new("select", &ScriptDialect::mysql(), Fold::None, false), "`select`");
        assert_eq!(quote_new("x", &pg, Fold::Lower, true), "\"x\"");
    }

    #[test]
    fn the_dependents_own_name_and_triggers() {
        // A trigger named like its table keeps its own name.
        let body = "CREATE TRIGGER Clientes ON dbo.Clientes AFTER UPDATE AS UPDATE c SET Pepe = i.Pepe FROM dbo.Clientes c JOIN inserted i ON i.id = c.id";
        let r = rw(body, &tsql(), &table("dbo", "Clientes"), "Nuevo");
        assert!(r.text.starts_with("CREATE TRIGGER Clientes ON dbo.Nuevo"), "{}", r.text);
        let r = rw(body, &tsql(), &column("dbo", "Clientes", "Pepe"), "Nuevo");
        assert!(r.text.ends_with("SET Nuevo = i.Nuevo FROM dbo.Clientes c JOIN inserted i ON i.id = c.id"), "{}", r.text);
    }

    #[test]
    fn view_column_lists_with_types_are_matched_by_name() {
        let ch = ScriptDialect::generic();
        let t = column("db", "t", "pepe");
        // ClickHouse stores the structure: the select keeps its output names.
        let body = "CREATE VIEW db.v (`id` UInt64, `pepe` String) AS SELECT id, pepe FROM db.t";
        let r = view(body, &ch, &t, "nuevo");
        assert_eq!(r.text, "CREATE VIEW db.v (`id` UInt64, `pepe` String) AS SELECT id, nuevo AS pepe FROM db.t");
        // A list of names is positional: no alias needed.
        let r = view("CREATE VIEW db.v (a, b) AS SELECT id, pepe FROM db.t", &ch, &t, "nuevo");
        assert_eq!(r.text, "CREATE VIEW db.v (a, b) AS SELECT id, nuevo FROM db.t");
        // A materialized view with TO: its list comes after the target table.
        let body = "CREATE MATERIALIZED VIEW db.mv TO db.dest (`id` UInt64, `pepe` String) AS SELECT id, pepe FROM db.t";
        assert!(view(body, &ch, &t, "nuevo").text.ends_with("SELECT id, nuevo AS pepe FROM db.t"));
    }

    #[test]
    fn keyword_qualifiers_and_table_calls() {
        let g = ScriptDialect::generic();
        let t = column("default", "s", "pepe");
        let r = view("CREATE VIEW default.v AS SELECT id, pepe FROM default.s", &g, &t, "nuevo");
        assert_eq!(r.text, "CREATE VIEW default.v AS SELECT id, nuevo AS pepe FROM default.s");
        // Proton's table(s): the stream read as a table.
        let r = view("CREATE VIEW v AS SELECT id, pepe FROM table(default.s) AS x WHERE x.pepe > ''", &g, &t, "nuevo");
        assert_eq!(r.text, "CREATE VIEW v AS SELECT id, nuevo AS pepe FROM table(default.s) AS x WHERE x.nuevo > ''");
        let r = rw("SELECT * FROM table(default.s)", &g, &table("default", "s"), "s2");
        assert_eq!(r.text, "SELECT * FROM table(default.s2)");
    }

    #[test]
    fn the_database_qualifies_a_target_without_schema() {
        let g = ScriptDialect::generic();
        let opts = RewriteOptions { database: Some("db".into()), ..Default::default() };
        let r = rewrite_references("SELECT * FROM db.t JOIN otra.t ON 1 = 1", &g, &table("", "t"), "n", &spec(Fold::None), &opts);
        assert_eq!(r.text, "SELECT * FROM db.n JOIN otra.t ON 1 = 1");
        assert_eq!(r.unresolved.len(), 1);
        assert_eq!(r.unresolved[0].reason, UnresolvedReason::Qualified);
    }

    #[test]
    fn trigger_rows() {
        let up = spec(Fold::Upper);
        let none = RewriteOptions::default();
        // Firebird: FOR <table>.
        let fb = ScriptDialect::generic();
        let body = "CREATE TRIGGER RN_TRG FOR RN_T BEFORE INSERT AS\nBEGIN\n  IF (NEW.PEPE IS NULL) THEN NEW.PEPE = 0;\nEND";
        let r = rewrite_references(body, &fb, &column("", "RN_T", "PEPE"), "NOMBRE", &up, &none);
        assert_eq!(r.text, body.replace("PEPE", "NOMBRE"));
        let body = "CREATE OR ALTER TRIGGER RN_TRG ACTIVE BEFORE INSERT ON RN_T AS BEGIN NEW.PEPE = OLD.PEPE; END";
        assert_eq!(rewrite_references(body, &fb, &column("", "RN_T", "PEPE"), "NOMBRE", &up, &none).text, body.replace("PEPE", "NOMBRE"));
        // Another table's trigger: left alone.
        let body = "CREATE TRIGGER X FOR OTRA BEFORE INSERT AS BEGIN NEW.PEPE = 0; END";
        assert!(rewrite_references(body, &fb, &column("", "RN_T", "PEPE"), "NOMBRE", &up, &none).edits.is_empty());
        // Oracle: :NEW, and the names REFERENCING gives.
        let ora = ScriptDialect::oracle();
        let body = "CREATE OR REPLACE TRIGGER APP.TG BEFORE INSERT OR UPDATE OF PEPE ON APP.T REFERENCING NEW AS N OLD AS O FOR EACH ROW\nBEGIN\n  :N.PEPE := :O.PEPE;\n  :NEW.PEPE := 1;\nEND;";
        let r = rewrite_references(body, &ora, &column("APP", "T", "PEPE"), "NOMBRE", &up, &none);
        assert_eq!(r.text, body.replace("PEPE", "NOMBRE"));
        // MySQL.
        let my = ScriptDialect::mysql();
        let body = "CREATE DEFINER=`root`@`%` TRIGGER `tg` BEFORE INSERT ON `t` FOR EACH ROW SET NEW.pepe = UPPER(NEW.pepe)";
        assert_eq!(rw(body, &my, &column("", "t", "pepe"), "nuevo").text, body.replace("NEW.pepe", "NEW.nuevo"));
        // A PostgreSQL trigger function: only when the app knows a trigger on the table runs it.
        let pg = ScriptDialect::postgres();
        let body = "CREATE OR REPLACE FUNCTION public.f() RETURNS trigger LANGUAGE plpgsql AS $$\nBEGIN\n  NEW.pepe := lower(NEW.pepe);\n  RETURN NEW;\nEND $$";
        let lo = spec(Fold::Lower);
        assert!(rewrite_references(body, &pg, &column("public", "t", "pepe"), "nuevo", &lo, &none).edits.is_empty());
        let rows = RewriteOptions { row_table: true, ..Default::default() };
        let r = rewrite_references(body, &pg, &column("public", "t", "pepe"), "nuevo", &lo, &rows);
        assert!(r.text.contains("NEW.nuevo := lower(NEW.nuevo);"), "{}", r.text);
    }

    #[test]
    fn triggers_and_their_routines() {
        let pg = ScriptDialect::postgres();
        let tg = "CREATE TRIGGER tg BEFORE INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION audit.f()";
        assert_eq!(trigger_routine(tg, &pg), Some((Some("audit".into()), "f".into())));
        assert_eq!(trigger_routine("CREATE TRIGGER tg AFTER UPDATE OF a ON t FOR EACH ROW EXECUTE PROCEDURE g()", &pg), Some((None, "g".into())));
        assert!(trigger_on(tg, &pg, "t") && !trigger_on(tg, &pg, "u"));
        assert!(trigger_routine("CREATE VIEW v AS SELECT 1", &pg).is_none());
        assert!(names_in_code("SELECT a FROM t -- pepe", &pg, "a") && !names_in_code("SELECT a FROM t -- pepe", &pg, "pepe"));
        let g = ScriptDialect::generic();
        assert!(writes_to_table("CREATE MATERIALIZED VIEW db.mv TO db.dest (`a` UInt8) AS SELECT 1 AS a", &g));
        assert!(!writes_to_table("CREATE MATERIALIZED VIEW db.mv (`a` UInt8) ENGINE = MergeTree ORDER BY a AS SELECT 1 AS a", &g));
    }

    #[test]
    fn create_style_collapses_spacing() {
        let t = tsql();
        assert_eq!(with_create_style("CREATE      VIEW dbo.v AS SELECT 1", &t, ReplaceStyle::CreateOrAlter), "CREATE OR ALTER VIEW dbo.v AS SELECT 1");
        let once = with_create_style("CREATE   OR   ALTER\n  VIEW v AS SELECT 1", &t, ReplaceStyle::CreateOrAlter);
        assert_eq!(once, "CREATE OR ALTER VIEW v AS SELECT 1");
        assert_eq!(with_create_style(&once, &t, ReplaceStyle::CreateOrAlter), once);
    }

    #[test]
    fn per_kind_styles_and_epilogues() {
        let s = RenameSpec {
            replace: ReplaceStyle::DropCreate,
            replace_kinds: [("view".to_string(), ReplaceStyle::CreateOrReplace)].into(),
            epilogue: Some("BEGIN DBMS_UTILITY.COMPILE_SCHEMA({schema}, FALSE); END;".into()),
            ..Default::default()
        };
        assert_eq!(s.replace_for("view"), ReplaceStyle::CreateOrReplace);
        assert_eq!(s.replace_for("procedure"), ReplaceStyle::DropCreate);
        let col = RenameTarget::Column { table: obj(kinds::TABLE, "O'X", "t"), column: "c".into() };
        assert_eq!(s.epilogue_for(&col).as_deref(), Some("BEGIN DBMS_UTILITY.COMPILE_SCHEMA('O''X', FALSE); END;"));
        assert!(s.epilogue_for(&RenameTarget::Object { object: obj(kinds::TABLE, "", "t"), parent: None }).is_none());
        assert!(RenameSpec::default().epilogue_for(&col).is_none());
        // Older specs (a plugin host's manifest) read without the new fields.
        let old: RenameSpec = serde_json::from_str(r#"{"kinds":["table"],"replace":"drop_create"}"#).unwrap();
        assert!(old.replace_kinds.is_empty() && old.holds_rows.is_empty() && old.epilogue.is_none());
    }

    #[test]
    fn targets_map_to_dependency_targets() {
        let t = RenameTarget::Index { table: obj(kinds::TABLE, "dbo", "t"), index: "ix".into() };
        assert_eq!(t.dependency_target().object.kind, "index");
        assert_eq!(t.old_name(), "ix");
        let s = RenameSpec { indexes: true, kinds: vec!["table".into()], ..Default::default() };
        assert!(s.allows(&t));
        assert!(!s.allows(&RenameTarget::Column { table: obj(kinds::TABLE, "dbo", "t"), column: "c".into() }));
        let json = serde_json::to_string(&RenameTarget::Schema { database: None, schema: "v".into() }).unwrap();
        assert_eq!(json, r#"{"what":"schema","database":null,"schema":"v"}"#);
    }
}
