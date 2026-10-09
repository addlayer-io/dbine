//! SQL rules. Tokens come from the driver crate's own lexer
//! ([`name_tokens`]) on the units [`split_script`] cuts, so strings,
//! comments, `$$` bodies and quoted names (`"…"`, `[…]`, `` `…` ``) are
//! what the engine's tool takes them for.

use super::{Finding, Flavor};
use dbine_driver::sql::{expose_versioned, name_tokens, split_script, unsafe_statements, ScriptDialect, StatementKind, TokenKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum K {
    Word,
    Num,
    /// A quoted identifier.
    Name,
    Str,
    Punct,
}

#[derive(Debug, Clone)]
pub(super) struct Tok<'a> {
    pub k: K,
    /// As written (a quoted name with its quotes).
    pub text: &'a str,
    /// Byte offsets in the script.
    pub start: usize,
    pub end: usize,
    /// Parenthesis depth (a `(` and its `)` are at the outer one).
    pub depth: u32,
    /// The punctuation right before it, 0 after anything else.
    pub prev: u8,
}

impl Tok<'_> {
    /// `kw` as a keyword: not a variable, parameter or qualified name.
    pub fn is(&self, kw: &str) -> bool {
        self.k == K::Word && self.text.eq_ignore_ascii_case(kw) && !matches!(self.prev, b'@' | b'.' | b':' | b'#')
    }

    pub fn is_any(&self, kws: &[&str]) -> bool {
        kws.iter().any(|k| self.is(k))
    }

    pub fn p(&self, c: char) -> bool {
        self.k == K::Punct && self.text.len() == c.len_utf8() && self.text.starts_with(c)
    }

    /// A name: a word or a quoted identifier.
    pub fn named(&self) -> bool {
        matches!(self.k, K::Word | K::Name)
    }

    /// The name without its quotes, lowercase.
    pub fn ident(&self) -> String {
        let t = self.text;
        let t = if self.k == K::Name && t.len() >= 2 { t.get(1..t.len() - 1).unwrap_or(t) } else { t };
        t.to_lowercase()
    }
}

/// The body of a string token: without its quotes and any prefix (`N'…'`,
/// `E'…'`).
pub(super) fn str_body(raw: &str) -> &str {
    let open = raw.find(['\'', '"', '$']).unwrap_or(0);
    let q = raw.as_bytes().get(open).copied().unwrap_or(b'\'');
    let body = &raw[open..];
    if q == b'$' {
        // $tag$ … $tag$
        let tag_end = body[1..].find('$').map_or(body.len(), |p| p + 2);
        let inner = &body[tag_end.min(body.len())..];
        return inner.strip_suffix(&body[..tag_end.min(body.len())]).unwrap_or(inner);
    }
    let inner = &body[1.min(body.len())..];
    inner.strip_suffix(q as char).unwrap_or(inner)
}

/// The tokens of `text` (which starts at `base` in the script).
pub(super) fn tokens<'a>(text: &'a str, base: usize, d: &ScriptDialect) -> Vec<Tok<'a>> {
    let origin = text.as_ptr() as usize;
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut prev = 0u8;
    for nt in name_tokens(text, d) {
        let at = nt.text.as_ptr() as usize - origin;
        let (k, raw) = match nt.kind {
            // A quoted name comes unquoted: take its quotes back.
            TokenKind::Name if at != nt.start => (K::Name, &text[nt.start..(at + nt.text.len() + 1).min(text.len())]),
            TokenKind::Name if nt.text.as_bytes()[0].is_ascii_digit() => (K::Num, nt.text),
            TokenKind::Name => (K::Word, nt.text),
            TokenKind::String => (K::Str, nt.text),
            TokenKind::Punct => (K::Punct, nt.text),
        };
        if k == K::Punct && raw == ")" {
            depth = depth.saturating_sub(1);
        }
        out.push(Tok { k, text: raw, start: base + nt.start, end: base + nt.start + raw.len(), depth, prev });
        if k == K::Punct && raw == "(" {
            depth += 1;
        }
        prev = if k == K::Punct { raw.as_bytes()[0] } else { 0 };
    }
    out
}

/// Words that start a new statement at the depth of a query (T-SQL runs
/// statements one after the other without `;`).
const BLOCK_END: &[&str] = &[
    "select", "insert", "update", "delete", "merge", "create", "alter", "drop", "declare", "set", "exec", "execute", "print",
    "return", "if", "while", "begin", "truncate", "grant", "revoke", "call", "commit", "rollback", "union", "intersect", "except",
    "minus",
];

/// Where a WHERE condition ends.
const WHERE_END: &[&str] = &[
    "group", "having", "order", "limit", "offset", "fetch", "for", "option", "returning", "window", "qualify", "select", "insert",
    "update", "delete", "merge", "create", "alter", "drop", "declare", "set", "exec", "execute", "print", "return", "if", "while",
    "begin", "truncate", "union", "intersect", "except", "minus",
];

/// Clauses of a query block.
const CLAUSES: &[&str] = &[
    "from", "where", "group", "having", "order", "limit", "offset", "fetch", "window", "qualify", "for", "into", "option",
    "returning", "emit", "allow", "union", "intersect", "except", "minus", "connect", "start",
];

/// Where a clause or query block that starts at `from` (depth `d`) ends:
/// its parenthesis closes, a `;`, another clause or statement.
pub(super) fn clause_end(t: &[Tok], from: usize, d: u32, stops: &[&str]) -> usize {
    let mut j = from;
    while j < t.len() {
        let x = &t[j];
        // A `)` at depth `d` closes a `(` of this level; the one that ends
        // the block is outside it.
        if x.depth < d || (x.depth == d && x.p(';')) {
            return j;
        }
        if x.depth == d && x.is_any(stops) && !t.get(j + 1).is_some_and(|n| n.p('(') && !x.is("select")) {
            // FOR UPDATE / FOR SHARE belong to the query, not a new statement.
            let tail = x.is_any(&["update", "delete"]) && j > 0 && t[j - 1].is_any(&["for", "key"]);
            if !tail {
                return j;
            }
        }
        j += 1;
    }
    t.len()
}

/// The index of the `)` closing the `(` at `open`.
pub(super) fn closing(t: &[Tok], open: usize) -> usize {
    let d = t[open].depth;
    (open + 1..t.len()).find(|&j| t[j].depth == d && t[j].p(')')).unwrap_or(t.len())
}

/// A `SELECT` query block and its clauses (index ranges into the tokens).
pub(super) struct Block {
    pub select: usize,
    pub end: usize,
    pub depth: u32,
    pub clauses: Vec<(&'static str, usize, usize)>,
}

impl Block {
    pub fn clause(&self, name: &str) -> Option<(usize, usize)> {
        self.clauses.iter().find(|c| c.0 == name).map(|c| (c.1, c.2))
    }

    /// The select list: after `SELECT` and its modifiers, to the first clause.
    pub fn list(&self, t: &[Tok]) -> (usize, usize) {
        let mut a = self.select + 1;
        while a < self.end && t[a].is_any(&["distinct", "all", "distinctrow", "straight_join", "sql_no_cache", "sql_calc_found_rows"]) {
            a += 1;
        }
        if a < self.end && t[a].is("top") {
            a += 1;
            if a < self.end && t[a].p('(') {
                a = closing(t, a) + 1;
            } else {
                a += 1;
            }
            while a < self.end && t[a].is_any(&["percent", "with", "ties"]) {
                a += 1;
            }
        }
        let b = self.clauses.first().map_or(self.end, |c| c.1);
        (a.min(b), b)
    }
}

pub(super) fn blocks(t: &[Tok]) -> Vec<Block> {
    let mut out = Vec::new();
    for (i, x) in t.iter().enumerate() {
        if !x.is("select") {
            continue;
        }
        let d = x.depth;
        let end = clause_end(t, i + 1, d, BLOCK_END);
        let mut clauses: Vec<(&'static str, usize, usize)> = Vec::new();
        let mut j = i + 1;
        while j < end {
            if t[j].depth == d {
                let name = CLAUSES.iter().find(|c| t[j].is(c));
                // `a IS DISTINCT FROM b`, `WITHIN GROUP (…)` aren't clauses.
                let inner = t[j - 1].is("distinct") && t[j].is("from") || t[j - 1].is("within") && t[j].is("group");
                if let Some(name) = name.filter(|_| !inner) {
                    if let Some(last) = clauses.last_mut() {
                        last.2 = j;
                    }
                    clauses.push((name, j, end));
                }
            }
            j += 1;
        }
        out.push(Block { select: i, end, depth: d, clauses });
    }
    out
}

/// Comma-separated items of `a..b` at depth `d`, as index ranges.
pub(super) fn items(t: &[Tok], a: usize, b: usize, d: u32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut s = a;
    for j in a..b {
        if t[j].depth == d && t[j].p(',') {
            out.push((s, j));
            s = j + 1;
        }
    }
    if s < b {
        out.push((s, b));
    }
    out
}

pub(super) fn lint(script: &str, d: &ScriptDialect, flavor: Flavor, out: &mut Vec<Finding>) {
    let script = expose_versioned(script, d);
    let script = script.as_ref();
    for u in unsafe_statements(script, d) {
        out.push(Finding::new("dml-without-where", u.start, u.start + u.keyword.len()).param("keyword", u.keyword));
    }
    for unit in split_script(script, &d.statements()) {
        if unit.kind == StatementKind::ClientCommand {
            continue;
        }
        let t = tokens(&unit.text, unit.start, d);
        let bs = blocks(&t);
        generic(&t, &bs, out);
        match flavor {
            Flavor::Tsql => tsql(&t, out),
            Flavor::Postgres => {
                for_update(&t, false, out);
                serial(&t, out);
            }
            Flavor::Mysql => {
                for_update(&t, false, out);
                group_by(&t, &bs, out);
            }
            Flavor::Oracle => {
                for_update(&t, true, out);
                oracle(&t, &bs, out);
            }
            Flavor::Influxql => influx(&t, out),
            Flavor::Generic => {}
        }
    }
}

/// The SQL rules every engine gets. Also used for CQL (`cql.rs`) in part.
fn generic(t: &[Tok], bs: &[Block], out: &mut Vec<Finding>) {
    select_star(t, bs, out);
    for i in 0..t.len() {
        let x = &t[i];
        if x.is("not") && t.get(i + 1).is_some_and(|n| n.is("in")) && t.get(i + 2).is_some_and(|n| n.p('('))
            && t.get(i + 3).is_some_and(|n| n.is_any(&["select", "with"]))
        {
            out.push(Finding::new("not-in-subquery", x.start, t[i + 1].end));
        }
        leading_wildcard(t, i, out);
        if x.is("null") && i > 0 {
            equals_null(t, i, out);
        }
        if x.is("order") && t.get(i + 1).is_some_and(|n| n.is("by")) {
            order_by(t, i, out);
        }
        if x.is("insert") && !t.get(i + 1).is_some_and(|n| n.p('(')) {
            insert_columns(t, i, out);
        }
        if x.is("union") && !t.get(i + 1).is_some_and(|n| n.is_any(&["all", "distinct"])) {
            out.push(Finding::new("union-distinct", x.start, x.end));
        }
        if x.is("where") {
            functions_on_columns(t, i, out);
        }
    }
    for b in bs {
        if t.get(b.select + 1).is_some_and(|n| n.is("distinct")) && b.clause("group").is_some() {
            out.push(Finding::new("distinct-group-by", t[b.select + 1].start, t[b.select + 1].end));
        }
        cross_join(t, b, out);
    }
}

pub(super) fn select_star(t: &[Tok], bs: &[Block], out: &mut Vec<Finding>) {
    for b in bs {
        let (a, _) = b.list(t);
        let Some(star) = t.get(a).filter(|x| x.p('*')) else { continue };
        // EXISTS (SELECT * …) reads no column.
        let i = b.select;
        if i >= 2 && t[i - 1].p('(') && t[i - 2].is("exists") {
            continue;
        }
        out.push(Finding::new("select-star", star.start, star.end));
    }
}

pub(super) fn leading_wildcard(t: &[Tok], i: usize, out: &mut Vec<Finding>) {
    if !t[i].is_any(&["like", "ilike"]) {
        return;
    }
    let mut j = i + 1;
    // CONCAT('%', x)
    if t.get(j).is_some_and(|x| x.is("concat")) && t.get(j + 1).is_some_and(|x| x.p('(')) {
        j += 2;
    }
    // N'…'
    if t.get(j).is_some_and(|x| x.k == K::Word && x.text.eq_ignore_ascii_case("n")) && t.get(j + 1).is_some_and(|s| s.k == K::Str && s.start == t[j].end) {
        j += 1;
    }
    let Some(s) = t.get(j).filter(|s| s.k == K::Str) else { return };
    let body = str_body(s.text);
    let concat = body == "%" && (j > i + 1 || t.get(j + 1).is_some_and(|n| n.p('+') || n.p('|')));
    if (body.starts_with('%') && body != "%") || concat {
        out.push(Finding::new("leading-wildcard", s.start, s.end).param("pattern", s.text));
    }
}

/// `= NULL` / `<> NULL` / `!= NULL` in a condition (an assignment like
/// `SET x = NULL` or a parameter's default is fine).
fn equals_null(t: &[Tok], null: usize, out: &mut Vec<Finding>) {
    let op = null - 1;
    let start = if t[op].p('=') {
        match op.checked_sub(1).map(|p| &t[p]) {
            Some(p) if p.end == t[op].start && (p.p('<') || p.p('>') || p.p(':')) => return,
            Some(p) if p.end == t[op].start && p.p('!') => op - 1,
            _ => op,
        }
    } else if t[op].p('>') && op > 0 && t[op - 1].p('<') && t[op - 1].end == t[op].start {
        op - 1
    } else {
        return;
    };
    const COND: &[&str] = &["where", "on", "having", "when", "and", "or", "not", "if", "while"];
    const STOP: &[&str] = &[
        "set", "select", "values", "declare", "default", "update", "procedure", "proc", "function", "then", "else", "return", "exec",
        "execute", "by", "returns", "as",
    ];
    for k in (start.saturating_sub(200)..start).rev() {
        let x = &t[k];
        if x.p(';') || x.is_any(STOP) {
            return;
        }
        if x.is_any(COND) {
            out.push(Finding::new("equals-null", t[start].start, t[null].end));
            return;
        }
    }
}

fn order_by(t: &[Tok], i: usize, out: &mut Vec<Finding>) {
    let d = t[i].depth;
    let end = clause_end(t, i + 2, d, &["limit", "offset", "fetch", "for", "union", "intersect", "except", "minus", "option", "into"]);
    let mut random = false;
    for (a, b) in items(t, i + 2, end, d) {
        let x = &t[a];
        if x.k == K::Num && (a + 1 == b || t[a + 1].is_any(&["asc", "desc", "nulls"])) {
            out.push(Finding::new("order-by-ordinal", x.start, x.end).param("n", x.text));
        }
        // ORDER BY RAND() / RANDOM() / NEWID() / DBMS_RANDOM.VALUE
        let call = x.is_any(&["rand", "random", "newid"]) && t.get(a + 1).is_some_and(|n| n.p('('));
        let oracle = x.is("dbms_random") && t.get(a + 1).is_some_and(|n| n.p('.')) && t.get(a + 2).is_some_and(|n| n.text.eq_ignore_ascii_case("value"));
        if (call || oracle) && !random {
            random = true;
            let last = if call { closing(t, a + 1).min(t.len() - 1) } else { a + 2 };
            out.push(Finding::new("order-by-random", t[i].start, t[last].end).param("call", if call { format!("{}()", x.text) } else { "DBMS_RANDOM.VALUE".into() }));
        }
    }
}

fn insert_columns(t: &[Tok], i: usize, out: &mut Vec<Finding>) {
    let mut j = i + 1;
    while t.get(j).is_some_and(|x| x.is_any(&["into", "ignore", "low_priority", "delayed", "high_priority", "overwrite", "table"])) {
        j += 1;
    }
    // INSERT OR REPLACE / OR IGNORE (SQLite)
    if t.get(j).is_some_and(|x| x.is("or")) {
        j += 2;
        if t.get(j).is_some_and(|x| x.is("into")) {
            j += 1;
        }
    }
    // Oracle's multi-table INSERT ALL / FIRST.
    if t.get(j).is_none_or(|x| x.is_any(&["all", "first"])) {
        return;
    }
    // @table, #temp, ##temp
    while t.get(j).is_some_and(|x| x.p('@') || x.p('#')) {
        j += 1;
    }
    if !t.get(j).is_some_and(|x| x.named()) {
        return;
    }
    j += 1;
    while t.get(j).is_some_and(|x| x.p('.')) && t.get(j + 1).is_some_and(|x| x.named()) {
        j += 2;
    }
    let table_end = t[j - 1].end;
    if t.get(j).is_some_and(|x| x.is("as")) {
        j += 2;
    }
    if t.get(j).is_some_and(|x| x.is("partition")) && t.get(j + 1).is_some_and(|x| x.p('(')) {
        j = closing(t, j + 1) + 1;
    }
    let Some(x) = t.get(j) else { return };
    let bare = x.is_any(&["values", "value", "select", "with", "exec", "execute"])
        || (x.p('(') && t.get(j + 1).is_some_and(|n| n.is_any(&["select", "with", "values"])));
    if bare {
        out.push(Finding::new("insert-without-columns", t[i].start, table_end));
    }
}

/// Functions that hide a column from its index when wrapped around it in a
/// condition.
const FUNCTIONS: &[&str] = &[
    "upper", "lower", "ucase", "lcase", "trim", "ltrim", "rtrim", "year", "month", "day", "date", "datepart", "datename", "date_part",
    "date_trunc", "datetrunc", "trunc", "substring", "substr", "left", "right", "len", "length", "char_length", "cast", "convert",
    "coalesce", "isnull", "ifnull", "nvl", "to_char", "to_date", "to_number", "date_format", "strftime", "round", "floor", "ceiling",
    "ceil", "abs", "concat", "replace", "format", "datediff", "timestampdiff", "unix_timestamp", "from_unixtime", "hour", "minute",
];

/// Words in a function's arguments that aren't columns: date parts, types.
const NOT_COLUMN: &[&str] = &[
    "year", "yy", "yyyy", "quarter", "qq", "q", "month", "mm", "m", "dayofyear", "dy", "y", "day", "dd", "d", "week", "wk", "ww",
    "weekday", "dw", "hour", "hh", "minute", "mi", "n", "second", "ss", "s", "millisecond", "ms", "microsecond", "nanosecond", "as",
    "int", "integer", "bigint", "smallint", "varchar", "nvarchar", "char", "nchar", "date", "datetime", "datetime2", "time",
    "timestamp", "decimal", "numeric", "float", "real", "text", "null", "true", "false", "unsigned", "signed", "using", "from",
    "both", "leading", "trailing", "max", "epoch", "dow", "doy", "isodow", "century", "decade",
];

fn functions_on_columns(t: &[Tok], w: usize, out: &mut Vec<Finding>) {
    let d = t[w].depth;
    let end = clause_end(t, w + 1, d, WHERE_END);
    let mut j = w + 1;
    while j + 1 < end {
        let f = &t[j];
        if f.k == K::Word && f.prev != b'.' && f.prev != b'@' && FUNCTIONS.iter().any(|n| f.text.eq_ignore_ascii_case(n)) && t[j + 1].p('(') {
            let close = closing(t, j + 1);
            let column = (j + 2..close.min(t.len())).any(|k| {
                let x = &t[k];
                let callee = t.get(k + 1).is_some_and(|n| n.p('('));
                let var = matches!(x.prev, b'@' | b':' | b'$');
                (x.k == K::Name || x.k == K::Word && !NOT_COLUMN.iter().any(|n| x.text.eq_ignore_ascii_case(n))) && !callee && !var
                    // `CAST(x AS type)`: the type isn't a column.
                    && !(k > 0 && t[k - 1].is("as"))
            });
            let cmp = t.get(close + 1).is_some_and(|n| {
                n.p('=') || n.p('<') || n.p('>') || n.p('!') || n.is_any(&["like", "ilike", "in", "between", "not"])
            });
            if column && cmp {
                out.push(Finding::new("function-on-column", f.start, t[close].end).param("fn", f.text.to_uppercase()));
                j = close;
            }
        }
        j += 1;
    }
}

/// `FROM a, b` whose WHERE never ties `b` to the rest.
fn cross_join(t: &[Tok], b: &Block, out: &mut Vec<Finding>) {
    let Some((from, end)) = b.clause("from") else { return };
    let parts = items(t, from + 1, end, b.depth);
    if parts.len() < 2 {
        return;
    }
    // Table functions and LATERAL / UNNEST take their rows from the others.
    let lateral = parts.iter().any(|&(a, e)| {
        (a..e).any(|k| t[k].is_any(&["lateral", "unnest", "table", "join", "apply", "openjson", "flatten"]))
            || (t[a].named() && t.get(a + 1).is_some_and(|n| n.p('(')))
    });
    if lateral {
        return;
    }
    let qualifiers: Vec<String> = match b.clause("where") {
        None => Vec::new(),
        Some((w, we)) => (w + 1..we).filter(|&k| t[k].named() && t.get(k + 1).is_some_and(|n| n.p('.'))).map(|k| t[k].ident()).collect(),
    };
    let has_where = b.clause("where").is_some();
    if has_where && qualifiers.is_empty() {
        // Unqualified columns: no telling which table each one is from.
        return;
    }
    for (n, &(a, e)) in parts.iter().enumerate() {
        if has_where {
            let names: Vec<String> = (a..e).filter(|&k| t[k].named() && t[k].depth == b.depth && !t[k].is("as")).map(|k| t[k].ident()).collect();
            if names.iter().any(|x| qualifiers.contains(x)) {
                continue;
            }
        } else if n == 0 {
            continue;
        }
        // The comma before it (after it, for the first table).
        let comma = if n == 0 { e } else { a - 1 };
        let (from, to) = if n == 0 { (t[a].start, t[comma].end) } else { (t[comma].start, t[e - 1].end) };
        out.push(Finding::new("implicit-cross-join", from, to).param("table", t[a..e].iter().map(|x| x.text).collect::<Vec<_>>().join(" ")));
        if !has_where {
            break;
        }
    }
}

fn tsql(t: &[Tok], out: &mut Vec<Finding>) {
    for i in 0..t.len() {
        let x = &t[i];
        if x.is_any(&["nolock", "readuncommitted"]) && i > 0 && (t[i - 1].p('(') || t[i - 1].p(',')) {
            out.push(Finding::new("nolock", x.start, x.end).param("hint", x.text.to_uppercase()));
        }
        if x.is("read") && t.get(i + 1).is_some_and(|n| n.is("uncommitted")) && i > 0 && t[i - 1].is("level") {
            out.push(Finding::new("nolock", x.start, t[i + 1].end).param("hint", "READ UNCOMMITTED"));
        }
        if x.is("cursor") {
            let declared = (i.saturating_sub(3)..i).any(|k| t[k].is("declare"));
            let options = t.get(i + 1).is_some_and(|n| {
                n.is_any(&["for", "local", "global", "forward_only", "scroll", "static", "keyset", "dynamic", "fast_forward", "read_only", "insensitive"])
            });
            if declared || options {
                out.push(Finding::new("cursor", x.start, x.end));
            }
        }
        if x.is("set") && t.get(i + 1).is_some_and(|n| n.is("rowcount")) && !t.get(i + 2).is_some_and(|n| n.text == "0") {
            out.push(Finding::new("set-rowcount", x.start, t[i + 1].end));
        }
        if x.p('@') && t.get(i + 1).is_some_and(|n| n.p('@') && n.start == x.end)
            && t.get(i + 2).is_some_and(|n| n.text.eq_ignore_ascii_case("identity") && n.start == t[i + 1].end)
        {
            out.push(Finding::new("global-identity", x.start, t[i + 2].end));
        }
    }
    // CREATE / ALTER PROCEDURE: its name and its body (the rest of the unit).
    let mut j = 0;
    if !t.first().is_some_and(|x| x.is_any(&["create", "alter"])) {
        return;
    }
    j += 1;
    if t.get(j).is_some_and(|x| x.is("or")) {
        j += 2;
    }
    if !t.get(j).is_some_and(|x| x.is_any(&["proc", "procedure"])) {
        return;
    }
    j += 1;
    let mut name = j;
    while t.get(name + 1).is_some_and(|x| x.p('.')) && t.get(name + 2).is_some_and(|x| x.named()) {
        name += 2;
    }
    let Some(n) = t.get(name).filter(|x| x.named()) else { return };
    if n.ident().starts_with("sp_") {
        out.push(Finding::new("sp-prefix", n.start, n.end).param("name", n.text));
    }
    let nocount = (name..t.len()).any(|k| t[k].is("set") && t.get(k + 1).is_some_and(|x| x.is("nocount")) && t.get(k + 2).is_some_and(|x| x.is("on")));
    if !nocount {
        out.push(Finding::new("set-nocount", t[0].start, n.end).param("name", n.text));
    }
}

/// `FOR UPDATE` / `FOR SHARE` that waits for the lock: no `NOWAIT` or
/// `SKIP LOCKED` (`WAIT n` too, on Oracle).
fn for_update(t: &[Tok], oracle: bool, out: &mut Vec<Finding>) {
    if !t.first().is_some_and(|x| x.is_any(&["select", "with", "declare"]) || x.p('(')) {
        return;
    }
    for i in 0..t.len() {
        if !t[i].is("for") {
            continue;
        }
        let mut j = i + 1;
        while t.get(j).is_some_and(|x| x.is_any(&["no", "key"])) {
            j += 1;
        }
        if !t.get(j).is_some_and(|x| x.is_any(&["update", "share"])) {
            continue;
        }
        let end = (j + 1..t.len()).find(|&k| t[k].depth < t[i].depth || t[k].p(';')).unwrap_or(t.len());
        let waits = !(j + 1..end).any(|k| {
            t[k].is("nowait") || (t[k].is("skip") && t.get(k + 1).is_some_and(|x| x.is("locked"))) || (oracle && t[k].is("wait"))
        });
        if waits {
            out.push(Finding::new("for-update-wait", t[i].start, t[j].end).param("clause", format!("FOR {}", t[j].text.to_uppercase())));
        }
    }
}

fn serial(t: &[Tok], out: &mut Vec<Finding>) {
    if !t.first().is_some_and(|x| x.is_any(&["create", "alter"])) {
        return;
    }
    for i in 1..t.len() {
        let x = &t[i];
        if x.is_any(&["serial", "bigserial", "smallserial", "serial2", "serial4", "serial8"]) && (t[i - 1].named() || t[i - 1].is("type")) {
            out.push(Finding::new("serial-identity", x.start, x.end).param("type", x.text.to_lowercase()));
        }
    }
}

/// MySQL: plain columns of the select list that GROUP BY doesn't list.
fn group_by(t: &[Tok], bs: &[Block], out: &mut Vec<Finding>) {
    for b in bs {
        let Some((g, ge)) = b.clause("group") else { continue };
        let (la, lb) = b.list(t);
        let list = items(t, la, lb, b.depth);
        if list.iter().any(|&(a, e)| (a..e).any(|k| t[k].p('*') && t[k].depth == b.depth)) {
            continue;
        }
        let start = if t.get(g + 1).is_some_and(|x| x.is("by")) { g + 2 } else { g + 1 };
        let grouped: Vec<(usize, usize)> = items(t, start, ge, b.depth);
        let mut names = Vec::new();
        let mut ordinals = Vec::new();
        for &(a, e) in &grouped {
            if t[a].k == K::Num && a + 1 == e {
                ordinals.push(t[a].text.parse::<usize>().unwrap_or(0));
            }
            if let Some(path) = plain_column(t, a, e) {
                names.push(path);
            }
        }
        for (n, &(a, e)) in list.iter().enumerate() {
            // Only plain columns: an expression may be an aggregate.
            let Some(path) = plain_column_with_alias(t, a, e) else { continue };
            let covered = ordinals.contains(&(n + 1))
                || names.iter().any(|g| g.0 == path.0 || g.1 == path.1 || path.2.as_ref().is_some_and(|al| &g.1 == al));
            if !covered {
                out.push(Finding::new("group-by-nonaggregated", t[a].start, t[e - 1].end).param("column", t[a..e].iter().map(|x| x.text).collect::<String>()));
            }
        }
    }
}

/// `a.b.c` filling `a..e` exactly: (full path, last part).
fn plain_column(t: &[Tok], a: usize, e: usize) -> Option<(String, String)> {
    let mut k = a;
    let mut parts = Vec::new();
    loop {
        let x = t.get(k).filter(|x| x.named() && k < e)?;
        parts.push(x.ident());
        k += 1;
        if k < e && t[k].p('.') {
            k += 1;
            continue;
        }
        break;
    }
    (k == e).then(|| (parts.join("."), parts.last().cloned().unwrap_or_default()))
}

/// A plain column, optionally followed by `[AS] alias`.
fn plain_column_with_alias(t: &[Tok], a: usize, e: usize) -> Option<(String, String, Option<String>)> {
    if let Some((p, l)) = plain_column(t, a, e) {
        return Some((p, l, None));
    }
    let alias_at = e - 1;
    let col_end = if alias_at > a && t[alias_at - 1].is("as") { alias_at - 1 } else { alias_at };
    if col_end <= a || !t[alias_at].named() {
        return None;
    }
    plain_column(t, a, col_end).map(|(p, l)| (p, l, Some(t[alias_at].ident())))
}

fn oracle(t: &[Tok], bs: &[Block], out: &mut Vec<Finding>) {
    for b in bs {
        let (Some((w, we)), Some(_)) = (b.clause("where"), b.clause("order")) else { continue };
        if let Some(k) = (w + 1..we).find(|&k| t[k].is("rownum") && t[k].depth == b.depth) {
            out.push(Finding::new("rownum-order-by", t[k].start, t[k].end));
        }
    }
    for i in 0..t.len().saturating_sub(2) {
        if t[i].p('(') && t[i + 1].p('+') && t[i + 2].p(')') {
            out.push(Finding::new("outer-join-plus", t[i].start, t[i + 2].end));
        }
    }
}

/// InfluxQL: a DELETE without a time range, DROP SERIES / MEASUREMENT.
fn influx(t: &[Tok], out: &mut Vec<Finding>) {
    let Some(first) = t.first() else { return };
    if first.is("delete") {
        if let Some(w) = t.iter().position(|x| x.is("where")) {
            if !t[w..].iter().any(|x| x.named() && x.ident() == "time") {
                out.push(Finding::new("delete-without-time", first.start, t[w].end));
            }
        }
    }
    if first.is("drop") && t.get(1).is_some_and(|x| x.is_any(&["series", "measurement"])) {
        out.push(Finding::new("drop-series", first.start, t[1].end).param("what", t[1].text.to_uppercase()));
    }
}
