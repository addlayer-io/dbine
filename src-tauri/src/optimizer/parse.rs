//! A light structural read of SQL: the SELECT blocks of a script with their
//! clauses, FROM items, and the conditions split into conjuncts. Ranges are
//! token indexes (`a..b`, `b` excluded). It reads what the rules need and
//! marks the rest as `exotic`, so a rule never rewrites what it didn't
//! understand.

use super::lex::{lex, unquote, Flavor, Kind, Tok};
use std::ops::Range;

pub type R = Range<usize>;

/// Words that are never a name (a table alias, a column).
const RESERVED: &[&str] = &[
    "select", "from", "where", "and", "or", "not", "null", "true", "false", "is", "in", "exists", "between", "like", "ilike", "case", "when", "then",
    "else", "end", "as", "on", "join", "inner", "left", "right", "full", "outer", "cross", "natural", "using", "group", "order", "by", "having",
    "limit", "offset", "fetch", "union", "intersect", "except", "minus", "all", "any", "some", "distinct", "top", "into", "for", "window",
    "qualify", "with", "apply", "lateral", "values", "interval", "date", "time", "timestamp", "current_date", "current_timestamp", "current_time",
    "escape", "collate", "over", "partition", "set", "update", "delete", "insert", "option", "connect", "start", "prewhere", "sample", "final",
    "straight_join", "pivot", "unpivot", "tablesample", "use", "force", "ignore", "returning", "rownum", "level", "settings", "array", "global",
];

pub fn is_reserved(word: &str) -> bool {
    RESERVED.contains(&word.to_ascii_lowercase().as_str())
}

/// Clause keywords after the select list.
const CLAUSES: &[&str] = &["from", "where", "group", "having", "order", "limit", "offset", "fetch", "window", "qualify", "into", "for", "option", "connect", "start", "prewhere", "sample", "settings", "lock"];

const SET_OPS: &[&str] = &["union", "intersect", "except", "minus"];

/// Statements that start a new one in scripts without `;` (T-SQL batches).
const STATEMENT_STARTS: &[&str] = &["select", "insert", "update", "delete", "merge", "declare", "if", "begin", "print", "exec", "execute", "create", "alter", "drop", "use", "return", "go", "while", "truncate", "grant"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColRef {
    /// `schema.table.column` as written, unquoted.
    pub parts: Vec<String>,
    pub range: R,
}

impl ColRef {
    pub fn column(&self) -> &str {
        self.parts.last().map(String::as_str).unwrap_or("")
    }
    /// The table or alias it names (`a` in `a.col`).
    pub fn qualifier(&self) -> Option<&str> {
        (self.parts.len() >= 2).then(|| self.parts[self.parts.len() - 2].as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// The first item, or one after a comma.
    First,
    Comma,
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone)]
pub struct FromItem {
    /// `schema.table` as written, unquoted; `None` for a derived table.
    pub name: Option<Vec<String>>,
    pub alias: Option<String>,
    pub join: JoinKind,
    /// The ON condition.
    pub on: Option<R>,
    /// A derived table's `( … )`.
    pub subquery: Option<R>,
}

impl FromItem {
    /// What the query calls it: the alias, else the table's own name.
    pub fn label(&self) -> Option<&str> {
        self.alias.as_deref().or_else(|| self.name.as_ref().and_then(|n| n.last()).map(String::as_str))
    }
    pub fn table(&self) -> Option<&str> {
        self.name.as_ref().and_then(|n| n.last()).map(String::as_str)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Block {
    /// The SELECT token.
    pub select: usize,
    /// Past its last token.
    pub end: usize,
    pub depth: u32,
    pub distinct: Option<usize>,
    /// `DISTINCT ON (…)`, `TOP n`, `SELECT … INTO`, `FOR UPDATE`, … : rules that restructure the block leave it alone.
    pub top: bool,
    pub list: R,
    pub from_kw: Option<usize>,
    pub from: Option<R>,
    pub where_kw: Option<usize>,
    pub where_: Option<R>,
    pub group_by: Option<R>,
    pub having: Option<R>,
    pub order_by: Option<R>,
    pub limit: bool,
    pub exotic: bool,
    /// Next to UNION / INTERSECT / EXCEPT.
    pub set_op: bool,
    /// The parentheses wrapping exactly this block.
    pub parens: Option<(usize, usize)>,
}

pub struct Sql<'a> {
    pub src: &'a str,
    pub toks: Vec<Tok>,
    /// Each token's nesting level (a parenthesis has the level outside it).
    pub depth: Vec<u32>,
    /// For each `(`, its `)`.
    pub close: Vec<Option<usize>>,
    pub blocks: Vec<Block>,
}

impl<'a> Sql<'a> {
    pub fn parse(src: &'a str, flavor: Flavor) -> Self {
        let toks = lex(src, flavor);
        let mut depth = Vec::with_capacity(toks.len());
        let mut close = vec![None; toks.len()];
        let mut stack: Vec<usize> = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            match t.kind {
                Kind::LParen => {
                    depth.push(stack.len() as u32);
                    stack.push(i);
                }
                Kind::RParen => {
                    if let Some(o) = stack.pop() {
                        close[o] = Some(i);
                    }
                    depth.push(stack.len() as u32);
                }
                _ => depth.push(stack.len() as u32),
            }
        }
        let mut sql = Sql { src, toks, depth, close, blocks: Vec::new() };
        sql.blocks = (0..sql.toks.len()).filter(|&i| sql.kw(i, "select")).map(|i| sql.block(i)).collect();
        sql
    }

    pub fn len(&self) -> usize {
        self.toks.len()
    }

    pub fn text(&self, i: usize) -> &'a str {
        let t = self.toks[i];
        &self.src[t.start..t.end]
    }

    pub fn kind(&self, i: usize) -> Option<Kind> {
        self.toks.get(i).map(|t| t.kind)
    }

    pub fn kw(&self, i: usize, kw: &str) -> bool {
        self.toks.get(i).is_some_and(|t| t.kind == Kind::Word && self.src[t.start..t.end].eq_ignore_ascii_case(kw))
    }

    pub fn kw_any(&self, i: usize, kws: &[&str]) -> bool {
        kws.iter().any(|k| self.kw(i, k))
    }

    pub fn is(&self, i: usize, kind: Kind) -> bool {
        self.kind(i) == Some(kind)
    }

    pub fn op(&self, i: usize, op: &str) -> bool {
        self.is(i, Kind::Op) && self.text(i) == op
    }

    /// The source of tokens `r` (from the first one's start to the last one's end).
    pub fn slice(&self, r: R) -> &'a str {
        if r.start >= r.end || r.start >= self.toks.len() {
            return "";
        }
        &self.src[self.toks[r.start].start..self.toks[r.end - 1].end]
    }

    pub fn start_of(&self, i: usize) -> usize {
        self.toks.get(i).map_or(self.src.len(), |t| t.start)
    }

    pub fn end_of(&self, i: usize) -> usize {
        self.toks[i].end
    }

    /// A name token's value: a word that isn't reserved, or a quoted name.
    pub fn name(&self, i: usize) -> Option<String> {
        let t = self.toks.get(i)?;
        match t.kind {
            Kind::Ident => Some(unquote(self.text(i))),
            Kind::Word if !RESERVED.contains(&self.text(i).to_ascii_lowercase().as_str()) => Some(self.text(i).to_string()),
            _ => None,
        }
    }

    /// `a.b.c` starting at `i`: the parts and where it ends. Not a call
    /// (`f(…)`).
    pub fn dotted(&self, i: usize) -> Option<(Vec<String>, usize)> {
        let mut parts = vec![self.name(i)?];
        let mut j = i + 1;
        while self.is(j, Kind::Dot) {
            match self.name(j + 1).or_else(|| (self.is(j + 1, Kind::Word)).then(|| self.text(j + 1).to_string())) {
                Some(n) => parts.push(n),
                None => break,
            }
            j += 2;
        }
        Some((parts, j))
    }

    /// The column reference that is exactly `r`.
    pub fn col_ref(&self, r: R) -> Option<ColRef> {
        let (parts, end) = self.dotted(r.start)?;
        (end == r.end && !self.is(end, Kind::LParen) && !self.is(end, Kind::Str)).then(|| ColRef { parts, range: r })
    }

    /// Every column reference in `r` outside nested SELECTs.
    pub fn col_refs(&self, r: R) -> Vec<ColRef> {
        let mut out = Vec::new();
        let mut i = r.start;
        while i < r.end {
            if self.is(i, Kind::LParen) && self.opens_select(i) {
                i = self.close[i].map_or(r.end, |c| c + 1);
                continue;
            }
            // A part after a dot was taken with its name.
            if i > r.start && self.is(i - 1, Kind::Dot) {
                i += 1;
                continue;
            }
            if let Some((parts, end)) = self.dotted(i) {
                // `t.*`; a type after AS (`CAST(x AS INT)`) or an alias; EXTRACT's unit.
                if self.is(end, Kind::Dot)
                    || (i > r.start && self.kw(i - 1, "as"))
                    || (i > r.start && self.is(i - 1, Kind::LParen) && self.kw(end, "from"))
                {
                    i = end + 1;
                    continue;
                }
                let call = self.is(end, Kind::LParen);
                // `DATE '…'`, `x::type`: a type, not a column.
                let typed = self.is(end, Kind::Str) || (i > r.start && self.op(i - 1, "::"));
                if !call && !typed && end <= r.end {
                    out.push(ColRef { parts, range: i..end });
                }
                i = end.max(i + 1);
                continue;
            }
            i += 1;
        }
        out
    }

    /// `(` at `i` holds a SELECT (or a WITH … SELECT).
    pub fn opens_select(&self, i: usize) -> bool {
        self.is(i, Kind::LParen) && (self.kw(i + 1, "select") || self.kw(i + 1, "with"))
    }

    /// The block whose SELECT is token `i`.
    pub fn block_at(&self, select: usize) -> Option<&Block> {
        self.blocks.iter().find(|b| b.select == select)
    }

    /// The block exactly inside the parentheses opening at `open`.
    pub fn subquery(&self, open: usize) -> Option<&Block> {
        let b = self.block_at(open + 1)?;
        (b.parens == Some((open, self.close[open]?))).then_some(b)
    }

    /// `r` split where `at` says, at its own level (not inside parentheses
    /// or CASE … END).
    fn split(&self, r: R, at: impl Fn(usize) -> bool) -> Vec<R> {
        if r.start >= r.end {
            return Vec::new();
        }
        let base = self.depth[r.start];
        let mut out = Vec::new();
        let mut from = r.start;
        let mut case = 0u32;
        let mut between = 0u32;
        for i in r.clone() {
            if self.depth[i] != base {
                continue;
            }
            if self.kw(i, "case") {
                case += 1;
            } else if self.kw(i, "end") && case > 0 {
                case -= 1;
            } else if case == 0 && self.kw(i, "between") {
                between += 1;
            } else if case == 0 && self.kw(i, "and") && between > 0 {
                between -= 1;
            } else if case == 0 && at(i) {
                out.push(from..i);
                from = i + 1;
            }
        }
        out.push(from..r.end);
        out
    }

    pub fn items(&self, r: R) -> Vec<R> {
        self.split(r, |i| self.is(i, Kind::Comma))
    }

    pub fn disjuncts(&self, r: R) -> Vec<R> {
        self.split(r, |i| self.kw(i, "or"))
    }

    /// The AND-ed parts of a condition; the whole condition when it's an OR
    /// at its top (AND binds tighter, so its ANDs aren't top-level).
    pub fn conjuncts(&self, r: R) -> Vec<R> {
        if self.disjuncts(r.clone()).len() > 1 {
            return vec![r];
        }
        self.split(r, |i| self.kw(i, "and"))
    }

    /// `( … )` around the whole of `r`: the inside.
    pub fn unwrap_parens(&self, r: &R) -> Option<R> {
        (self.is(r.start, Kind::LParen) && self.close[r.start] == Some(r.end - 1) && r.end - r.start >= 2).then(|| r.start + 1..r.end - 1)
    }

    /// Calls of `names` in `r` (outside nested SELECTs): the name tokens.
    pub fn calls(&self, r: R, names: &[&str]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = r.start;
        while i < r.end {
            if self.opens_select(i) {
                i = self.close[i].map_or(r.end, |c| c + 1);
                continue;
            }
            if self.is(i + 1, Kind::LParen) && self.kw_any(i, names) && !(i > r.start && self.is(i - 1, Kind::Dot)) {
                out.push(i);
            }
            i += 1;
        }
        out
    }

    /// Any of `words` in `r`, at any depth.
    pub fn mentions(&self, r: R, words: &[&str]) -> bool {
        r.into_iter().any(|i| self.kw_any(i, words))
    }

    /// Whether `r` has a nested SELECT.
    pub fn has_subquery(&self, r: R) -> bool {
        r.into_iter().any(|i| self.opens_select(i))
    }

    fn block(&self, select: usize) -> Block {
        let d = self.depth[select];
        let n = self.toks.len();
        // Where it ends.
        let mut end = n;
        let mut case = 0u32;
        let mut j = select + 1;
        while j < n {
            if self.depth[j] < d {
                end = j;
                break;
            }
            if self.depth[j] == d {
                if self.kw(j, "case") {
                    case += 1;
                } else if self.kw(j, "end") && case > 0 {
                    case -= 1;
                } else if case == 0 {
                    // Not a function of the same name (MySQL's IF(…), INSERT(…)) nor `USE INDEX`, `FOR UPDATE`.
                    let starts = self.kw_any(j, STATEMENT_STARTS)
                        && !self.is(j + 1, Kind::LParen)
                        && !self.is(j - 1, Kind::LParen)
                        && !(self.kw(j, "use") && self.kw_any(j + 1, &["index", "key"]))
                        && !self.kw_any(j - 1, &["all", "distinct", "union", "intersect", "except", "minus", "for", "on"]);
                    // `WITH (NOLOCK)`, `WITH TIES`, `WITH ROLLUP`… belong to the block; `WITH x AS (` is a new statement.
                    let with = self.kw(j, "with")
                        && !self.is(j + 1, Kind::LParen)
                        && !self.kw_any(j + 1, &["ties", "rollup", "cube", "check", "time", "local", "ordinality", "offset", "nowait"])
                        && !self.kw(j - 1, "start");
                    if self.is(j, Kind::Semi) || self.kw_any(j, SET_OPS) || starts || with || self.kw(j, "end") {
                        end = j;
                        break;
                    }
                }
            }
            j += 1;
        }
        let mut b = Block { select, end, depth: d, ..Default::default() };
        // DISTINCT / ALL / TOP.
        let mut k = select + 1;
        if self.kw(k, "distinct") {
            b.distinct = Some(k);
            k += 1;
            if self.kw(k, "on") {
                b.top = true;
            }
        } else if self.kw(k, "all") {
            k += 1;
        }
        if self.kw(k, "top") {
            b.top = true;
            k += 1;
            if self.is(k, Kind::LParen) {
                k = self.close[k].map_or(k + 1, |c| c + 1);
            } else {
                k += 1;
            }
            if self.kw(k, "percent") {
                k += 1;
            }
            if self.kw(k, "with") && self.kw(k + 1, "ties") {
                k += 2;
            }
        }
        // Clauses at its level.
        let mut marks: Vec<(usize, &str)> = Vec::new();
        let mut case = 0u32;
        for i in k..end {
            if self.depth[i] != d {
                continue;
            }
            if self.kw(i, "case") {
                case += 1;
                continue;
            }
            if self.kw(i, "end") && case > 0 {
                case -= 1;
                continue;
            }
            if case > 0 {
                continue;
            }
            let Some(c) = CLAUSES.iter().find(|c| self.kw(i, c)) else { continue };
            // GROUP / ORDER only with BY (`WITHIN GROUP (…)`); START only with WITH.
            if (*c == "group" || *c == "order") && !self.kw(i + 1, "by") {
                continue;
            }
            if *c == "start" && !self.kw(i + 1, "with") {
                continue;
            }
            marks.push((i, c));
        }
        b.list = k..marks.first().map_or(end, |m| m.0);
        for (n, &(i, c)) in marks.iter().enumerate() {
            let until = marks.get(n + 1).map_or(end, |m| m.0);
            match c {
                "from" if b.from.is_none() => {
                    b.from_kw = Some(i);
                    b.from = Some(i + 1..until);
                }
                "where" if b.where_.is_none() => {
                    b.where_kw = Some(i);
                    b.where_ = Some(i + 1..until);
                }
                "group" => b.group_by = Some(i + 2..until),
                "having" => b.having = Some(i + 1..until),
                "order" => b.order_by = Some(i + 2..until),
                "limit" | "offset" | "fetch" => b.limit = true,
                _ => b.exotic = true,
            }
        }
        // Oracle's ROWNUM limits rows too.
        if self.mentions(b.list.start..end, &["rownum"]) {
            b.limit = true;
        }
        if select > 0 && self.is(select - 1, Kind::LParen) && self.close[select - 1] == Some(end) {
            b.parens = Some((select - 1, end));
        }
        let (before, after) = match b.parens {
            Some((o, c)) => (o.checked_sub(1), c + 1),
            None => (select.checked_sub(1), end),
        };
        let near_set_op = |i: usize| self.kw_any(i, SET_OPS) || ((self.kw(i, "all") || self.kw(i, "distinct")) && i > 0 && self.kw_any(i - 1, SET_OPS));
        b.set_op = before.is_some_and(near_set_op) || near_set_op(after);
        b
    }

    /// The FROM items of a block, or `None` when the FROM has something the
    /// rules don't read (APPLY, PIVOT, table functions, USING…).
    pub fn from_items(&self, b: &Block) -> Option<Vec<FromItem>> {
        let r = b.from.clone()?;
        let mut out: Vec<FromItem> = Vec::new();
        let mut i = r.start;
        let mut join = JoinKind::First;
        while i < r.end {
            let start = i;
            let (name, subquery) = if self.is(i, Kind::LParen) {
                let c = self.close[i]?;
                if !self.opens_select(i) || c >= r.end {
                    return None;
                }
                i = c + 1;
                (None, Some(start..c + 1))
            } else {
                let (parts, e) = self.dotted(i)?;
                if self.is(e, Kind::LParen) {
                    return None;
                }
                i = e;
                (Some(parts), None)
            };
            let mut alias = None;
            if self.kw(i, "as") {
                alias = Some(self.name(i + 1)?);
                i += 2;
            } else if let Some(a) = self.name(i).filter(|_| i < r.end) {
                alias = Some(a);
                i += 1;
            }
            // T-SQL table hints.
            if self.kw(i, "with") && self.is(i + 1, Kind::LParen) {
                i = self.close[i + 1]? + 1;
            }
            let mut item = FromItem { name, alias, join, on: None, subquery };
            // Its ON, up to the next join.
            if self.kw(i, "on") {
                let on_start = i + 1;
                let mut j = on_start;
                while j < r.end && !(self.depth[j] == self.depth[r.start] && (self.is(j, Kind::Comma) || self.join_word(j))) {
                    j += 1;
                }
                item.on = Some(on_start..j);
                i = j;
            } else if self.kw(i, "using") {
                return None;
            }
            out.push(item);
            if i >= r.end {
                break;
            }
            // What joins the next one.
            if self.is(i, Kind::Comma) {
                join = JoinKind::Comma;
                i += 1;
                continue;
            }
            let mut kind = JoinKind::Inner;
            while i < r.end && self.join_word(i) && !self.kw(i, "join") {
                if self.kw(i, "left") {
                    kind = JoinKind::Left;
                } else if self.kw(i, "right") {
                    kind = JoinKind::Right;
                } else if self.kw(i, "full") {
                    kind = JoinKind::Full;
                } else if self.kw(i, "cross") {
                    kind = JoinKind::Cross;
                } else if self.kw(i, "natural") {
                    return None;
                }
                i += 1;
            }
            if !self.kw(i, "join") {
                return None;
            }
            join = kind;
            i += 1;
        }
        // An inner join needs its ON, a cross join none.
        for it in &out {
            match it.join {
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full if it.on.is_none() => return None,
                JoinKind::Cross if it.on.is_some() => return None,
                _ => {}
            }
        }
        Some(out)
    }

    fn join_word(&self, i: usize) -> bool {
        self.kw_any(i, &["join", "inner", "left", "right", "full", "outer", "cross", "natural"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(s: &str) -> Sql<'_> {
        Sql::parse(s, Flavor::default())
    }

    #[test]
    fn blocks_and_clauses() {
        let s = sql("SELECT a, b FROM t WHERE x = 1 AND y IN (SELECT y FROM u) ORDER BY a LIMIT 3");
        assert_eq!(s.blocks.len(), 2);
        let b = &s.blocks[0];
        assert_eq!(s.slice(b.list.clone()), "a, b");
        assert_eq!(s.slice(b.from.clone().unwrap()), "t");
        assert_eq!(s.slice(b.where_.clone().unwrap()), "x = 1 AND y IN (SELECT y FROM u)");
        assert_eq!(s.slice(b.order_by.clone().unwrap()), "a");
        assert!(b.limit);
        let inner = &s.blocks[1];
        assert_eq!(s.slice(inner.select..inner.end), "SELECT y FROM u");
        assert!(inner.parens.is_some());
    }

    #[test]
    fn conjuncts_respect_or_between_and_case() {
        let s = sql("SELECT 1 FROM t WHERE a BETWEEN 1 AND 2 AND (b = 1 OR c = 2) AND CASE WHEN d = 1 AND e = 2 THEN 1 END = 1");
        let w = s.blocks[0].where_.clone().unwrap();
        let parts: Vec<&str> = s.conjuncts(w).into_iter().map(|r| s.slice(r)).collect();
        assert_eq!(parts, vec!["a BETWEEN 1 AND 2", "(b = 1 OR c = 2)", "CASE WHEN d = 1 AND e = 2 THEN 1 END = 1"]);
        let s = sql("SELECT 1 FROM t WHERE a = 1 AND b = 2 OR c = 3");
        let w = s.blocks[0].where_.clone().unwrap();
        assert_eq!(s.conjuncts(w.clone()).len(), 1);
        assert_eq!(s.disjuncts(w).len(), 2);
    }

    #[test]
    fn from_items_with_joins_and_aliases() {
        let s = sql("SELECT * FROM sales.orders o JOIN customers AS c ON c.id = o.cid LEFT OUTER JOIN x ON x.k = o.k, y WHERE 1 = 1");
        let items = s.from_items(&s.blocks[0]).unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].name.as_deref(), Some(&["sales".to_string(), "orders".to_string()][..]));
        assert_eq!(items[0].alias.as_deref(), Some("o"));
        assert_eq!(items[1].label(), Some("c"));
        assert_eq!(s.slice(items[1].on.clone().unwrap()), "c.id = o.cid");
        assert_eq!(items[2].join, JoinKind::Left);
        assert_eq!(items[3].join, JoinKind::Comma);
        assert_eq!(items[3].label(), Some("y"));
    }

    #[test]
    fn from_items_refuse_what_they_dont_read() {
        let s = sql("SELECT * FROM a CROSS APPLY f(a.x)");
        assert!(s.from_items(&s.blocks[0]).is_none());
        let s = sql("SELECT * FROM a JOIN b USING (k)");
        assert!(s.from_items(&s.blocks[0]).is_none());
    }

    #[test]
    fn tsql_batches_without_semicolons_end_blocks() {
        let s = Sql::parse("SELECT a FROM t WHERE x = 1\nSELECT b FROM u", Flavor::for_dialect("mssql"));
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(s.slice(s.blocks[0].select..s.blocks[0].end), "SELECT a FROM t WHERE x = 1");
        let s = Sql::parse("SELECT TOP (5) a FROM t WITH (NOLOCK) WHERE x = 1", Flavor::for_dialect("mssql"));
        assert!(s.blocks[0].top);
        assert_eq!(s.slice(s.blocks[0].where_.clone().unwrap()), "x = 1");
        assert_eq!(s.from_items(&s.blocks[0]).unwrap()[0].label(), Some("t"));
    }

    #[test]
    fn set_ops_and_within_group() {
        let s = sql("SELECT a FROM t UNION ALL SELECT a FROM u");
        assert!(s.blocks.iter().all(|b| b.set_op));
        let s = sql("SELECT LISTAGG(a) WITHIN GROUP (ORDER BY a) FROM t");
        assert!(s.blocks[0].group_by.is_none() && s.blocks[0].order_by.is_none());
    }

    #[test]
    fn column_refs_skip_calls_types_and_subqueries() {
        let s = sql("SELECT 1 FROM t WHERE o.a = f(b) AND DATE '2024-01-01' < c AND d IN (SELECT e FROM u)");
        let w = s.blocks[0].where_.clone().unwrap();
        let refs: Vec<String> = s.col_refs(w).into_iter().map(|c| c.parts.join(".")).collect();
        assert_eq!(refs, vec!["o.a", "b", "c", "d"]);
    }
}
