//! What the schema compare needs beyond information_schema: parts of
//! `SHOW CREATE TABLE` (full-text parsers, TiDB's CHECKs, GreptimeDB's
//! column indexes, StarRocks / Doris properties, Manticore settings) and
//! the index clauses the DDL writers share.

use dbine_driver::{CheckDef, IndexDef};
use std::collections::BTreeMap;

/// Suffix of a CHECK the server doesn't enforce (MySQL, TiDB).
pub(crate) const NOT_ENFORCED: &str = " NOT ENFORCED";

/// How a MariaDB column CHECK rides in the column's type: `int(11) CHECK (…)`.
pub(crate) const COLUMN_CHECK: &str = " CHECK (";

/// A column type without its column CHECK, and the CHECK clause.
pub(crate) fn split_column_check(ty: &str) -> (&str, Option<&str>) {
    match ty.rfind(COLUMN_CHECK) {
        Some(at) if ty.ends_with(')') && inside(ty, at + COLUMN_CHECK.len() - 1).is_some_and(|b| at + COLUMN_CHECK.len() + b.len() + 1 == ty.len()) => {
            (&ty[..at], Some(ty[at..].trim_start()))
        }
        _ => (ty, None),
    }
}

/// Split on commas outside parentheses and quotes.
pub(crate) fn split_top(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == '\\' && q != '`' {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => out.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            },
        }
    }
    out.push(cur);
    out.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

/// The text between the `(` at `open` and its matching `)`.
pub(crate) fn inside(s: &str, open: usize) -> Option<&str> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut prev = '\0';
    for (i, c) in s[open..].char_indices() {
        match quote {
            Some(q) => {
                if c == q && prev != '\\' {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[open + 1..open + i]);
                    }
                }
                _ => {}
            },
        }
        prev = c;
    }
    None
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['\'', '"', '`'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].replace(&format!("\\{q}"), &q.to_string()).replace(&format!("{q}{q}"), &q.to_string());
        }
    }
    s.to_string()
}

/// `"k" = "v", k2 = 'v2'` (with or without the parentheses) as a map.
pub(crate) fn key_values(s: &str) -> BTreeMap<String, String> {
    let s = s.trim();
    let s = if s.starts_with('(') { inside(s, 0).unwrap_or(s) } else { s };
    split_top(s)
        .iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((unquote(k), unquote(v)))
        })
        .collect()
}

/// An index's descending key parts (lowercase), from `options["desc"]`.
pub(crate) fn desc_parts(ix: &IndexDef) -> Vec<String> {
    ix.options.get("desc").map(|d| split_top(d).into_iter().map(|c| c.to_lowercase()).collect()).unwrap_or_default()
}

/// `name`, `name(10)` or an expression `(lower(name))`, quoted for MySQL,
/// with `DESC` when the index says so.
pub(crate) fn key_part(ix: &IndexDef, c: &str) -> String {
    let desc = desc_parts(ix).contains(&c.to_lowercase());
    let part = if c.starts_with('(') {
        c.to_string()
    } else if let Some((name, len)) = c.strip_suffix(')').and_then(|s| s.rsplit_once('(')).filter(|(_, l)| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit())) {
        format!("{}({len})", quote(name))
    } else {
        quote(c)
    };
    if desc { format!("{part} DESC") } else { part }
}

fn quote(name: &str) -> String {
    dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Backtick, name)
}

/// `WITH PARSER` of each full-text index in a MySQL / MariaDB `SHOW
/// CREATE TABLE` (`FULLTEXT KEY `ft` (`b`) /*!50100 WITH PARSER `ngram` */`).
pub(crate) fn fulltext_parsers(create: &str) -> Vec<(String, String)> {
    create
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("FULLTEXT KEY ").or_else(|| l.strip_prefix("FULLTEXT INDEX "))?;
            let name = unquote(rest.split_whitespace().next()?);
            let p = &l[l.find("WITH PARSER ")? + "WITH PARSER ".len()..];
            let parser = unquote(p.split_whitespace().next()?.trim_end_matches(','));
            Some((name, parser))
        })
        .collect()
}

/// The CHECKs of a `SHOW CREATE TABLE` (TiDB, whose catalog doesn't say
/// which table a CHECK belongs to), as information_schema writes them.
pub(crate) fn create_checks(create: &str) -> Vec<CheckDef> {
    logical_lines(create)
        .into_iter()
        .filter_map(|l| {
            let l = l.trim().trim_end_matches(',');
            let rest = l.strip_prefix("CONSTRAINT ")?;
            let (name, rest) = split_name(rest)?;
            let rest = rest.trim_start();
            let at = rest.find('(').filter(|_| rest.starts_with("CHECK"))?;
            let body = inside(rest, at)?;
            let tail = rest[at + body.len() + 2..].to_ascii_uppercase();
            // TiDB writes CHECK ((expr)); information_schema says (expr).
            let expr = if body.starts_with('(') && inside(body, 0).is_some_and(|b| b.len() + 2 == body.len()) { body.to_string() } else { format!("({body})") };
            let off = if tail.contains("NOT ENFORCED") { NOT_ENFORCED } else { "" };
            Some(CheckDef { name: Some(unquote(name)), expression: format!("{expr}{off}") })
        })
        .collect()
}

/// The lines of a `SHOW CREATE TABLE`, split only at newlines outside
/// quotes: TiDB prints a newline inside a string literal as it is, and a
/// literal's next "line" is not a clause of the table.
fn logical_lines(create: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in create.char_indices() {
        match quote {
            Some(_) if escaped => escaped = false,
            Some(q) if c == '\\' && q != '`' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '\n' => {
                    out.push(&create[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(&create[start..]);
    out
}

/// A leading identifier (backtick-quoted, with `` inside and any spaces,
/// or bare up to a space) and what follows it.
fn split_name(s: &str) -> Option<(&str, &str)> {
    if !s.starts_with('`') {
        return s.split_once(' ');
    }
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() {
        if b[i] == b'`' {
            if b.get(i + 1) == Some(&b'`') {
                i += 2;
                continue;
            }
            return Some((&s[..=i], &s[i + 1..]));
        }
        i += 1;
    }
    None
}

/// A CHECK's clause with `NOT ENFORCED` where it goes: after the condition.
/// `check_clause` wraps the whole stored text in parentheses.
pub(crate) fn place_not_enforced(sql: &str, checks: &[CheckDef]) -> String {
    let mut s = sql.to_string();
    for c in checks {
        let e = c.expression.trim();
        if let Some(cond) = e.strip_suffix(NOT_ENFORCED.trim_start()).map(str::trim_end) {
            s = s.replace(&format!("CHECK ({e})"), &format!("CHECK {}{NOT_ENFORCED}", paren(cond)));
        }
    }
    s
}

fn paren(e: &str) -> String {
    if e.starts_with('(') && inside(e, 0).is_some_and(|b| b.len() + 2 == e.len()) { e.to_string() } else { format!("({e})") }
}

/// GreptimeDB's column indexes in `SHOW CREATE TABLE`: (column, kind, WITH options).
pub(crate) fn greptime_indexes(create: &str) -> Vec<(String, String, BTreeMap<String, String>)> {
    let mut out = Vec::new();
    for l in create.lines() {
        let l = l.trim();
        if !l.starts_with('`') {
            continue;
        }
        let Some(end) = l[1..].find('`') else { continue };
        let col = l[1..end + 1].to_string();
        let upper = l.to_ascii_uppercase();
        for kind in ["INVERTED", "FULLTEXT", "SKIPPING"] {
            let Some(at) = upper.find(&format!("{kind} INDEX")) else { continue };
            let after = &l[at + kind.len() + " INDEX".len()..];
            let opts = match after.trim_start().strip_prefix("WITH").map(str::trim_start) {
                Some(w) if w.starts_with('(') => inside(w, 0).map(key_values).unwrap_or_default(),
                _ => BTreeMap::new(),
            };
            out.push((col.clone(), kind.to_string(), opts));
        }
    }
    out
}

/// `NGRAMBF("gram_num" = "4", …)` (StarRocks' SHOW INDEX) as kind and options.
pub(crate) fn olap_index_type(t: &str) -> (Option<String>, BTreeMap<String, String>) {
    let t = t.trim();
    match t.find('(') {
        Some(at) => (Some(t[..at].trim().to_string()), key_values(&t[at..])),
        None => ((!t.is_empty()).then(|| t.to_string()), BTreeMap::new()),
    }
}

/// `PROPERTIES (…)` of a StarRocks / Doris `SHOW CREATE TABLE`.
pub(crate) fn olap_properties(create: &str) -> BTreeMap<String, String> {
    match create.find("PROPERTIES (").or_else(|| create.find("PROPERTIES(")) {
        Some(at) => {
            let open = at + create[at..].find('(').unwrap_or(0);
            inside(create, open).map(key_values).unwrap_or_default()
        }
        None => BTreeMap::new(),
    }
}

/// A column list compared as a set: trimmed, sorted, `, `-joined.
pub(crate) fn column_set(s: &str) -> String {
    let mut v: Vec<String> = s.split(',').map(|c| c.trim().trim_matches('`').to_string()).filter(|c| !c.is_empty()).collect();
    v.sort();
    v.join(", ")
}

/// Manticore's table settings after the column list (`morphology='stem_en'`).
pub(crate) fn manticore_settings(create: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(close) = create.find('(').and_then(|open| inside(create, open).map(|b| open + b.len() + 2)) else { return out };
    let mut rest = create[close..].trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().to_string();
        let v = rest[eq + 1..].trim_start();
        if !v.starts_with('\'') {
            break;
        }
        // Quoted with backslash escapes.
        let mut val = String::new();
        let mut end = None;
        let mut chars = v.char_indices().skip(1);
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, n)) = chars.next() {
                        val.push(n);
                    }
                }
                '\'' => {
                    end = Some(i);
                    break;
                }
                _ => val.push(c),
            }
        }
        let Some(end) = end else { break };
        if !key.is_empty() {
            out.insert(key, val);
        }
        rest = v[end + 1..].trim_start();
    }
    out
}

/// A Manticore setting value: quoted with backslash escapes.
pub(crate) fn manticore_setting(k: &str, v: &str) -> String {
    format!("{k}='{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// GreptimeDB's table options: `WITH(ttl = '7days', 'compaction.type' = 'twcs')`
/// after `ENGINE=mito`, without the comment (read apart).
pub(crate) fn greptime_with(create: &str) -> BTreeMap<String, String> {
    let Some(engine) = create.find("ENGINE=") else { return BTreeMap::new() };
    let rest = &create[engine..];
    let Some(at) = rest.find("WITH(").or_else(|| rest.find("WITH (")) else { return BTreeMap::new() };
    let open = engine + at + rest[at..].find('(').unwrap_or(0);
    let mut kv = inside(create, open).map(key_values).unwrap_or_default();
    kv.remove("comment");
    kv
}

/// A GreptimeDB option key, quoted when it isn't a bare word (`'compaction.type'`).
pub(crate) fn greptime_key(k: &str) -> String {
    if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') { k.to_string() } else { format!("'{}'", k.replace('\'', "''")) }
}

/// SingleStore's keys in `SHOW CREATE TABLE`: shard and sort keys (table
/// settings, not indexes), hash and full-text indexes, and the table type.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct SingleStoreKeys {
    /// `ROWSTORE`, `REFERENCE`, `ROWSTORE REFERENCE`… (`None` = columnstore).
    pub table_type: Option<String>,
    /// (name, key parts as written).
    pub shard: Option<(String, String)>,
    pub sort: Option<(String, String)>,
    pub hash: Vec<String>,
    /// (name, `USING VERSION` n).
    pub fulltext: Vec<(String, Option<String>)>,
}

pub(crate) fn singlestore_keys(create: &str) -> SingleStoreKeys {
    let mut k = SingleStoreKeys::default();
    let head = create.trim_start();
    if let Some(rest) = head.strip_prefix("CREATE ") {
        let ty = rest.split(" TABLE").next().unwrap_or("").trim();
        if !ty.is_empty() && !ty.contains('`') && !ty.contains('(') {
            k.table_type = Some(ty.to_ascii_uppercase());
        }
    }
    let name_and_parts = |rest: &str| -> Option<(String, String)> {
        let open = rest.find('(')?;
        let name = unquote(rest[..open].trim());
        Some((name, inside(rest, open)?.trim().to_string()))
    };
    for l in create.lines() {
        let l = l.trim().trim_end_matches(',');
        if let Some(rest) = l.strip_prefix("SHARD KEY") {
            k.shard = name_and_parts(rest);
        } else if let Some(rest) = l.strip_prefix("SORT KEY") {
            k.sort = name_and_parts(rest);
        } else if l.starts_with("KEY ") && l.ends_with("USING CLUSTERED COLUMNSTORE") {
            // The sort key's older spelling.
            k.sort = name_and_parts(&l[4..]);
        } else if (l.starts_with("KEY ") || l.starts_with("UNIQUE KEY ")) && l.ends_with("USING HASH") {
            let rest = l.strip_prefix("UNIQUE ").unwrap_or(l);
            if let Some((n, _)) = name_and_parts(&rest[4..]) {
                k.hash.push(n);
            }
        } else if let Some(rest) = l.strip_prefix("FULLTEXT ") {
            let (version, rest) = match rest.strip_prefix("USING VERSION ") {
                Some(r) => {
                    let (v, r) = r.split_once(' ').unwrap_or((r, ""));
                    (Some(v.to_string()), r)
                }
                None => (None, rest),
            };
            let rest = rest.strip_prefix("KEY ").unwrap_or(rest);
            if let Some((n, _)) = name_and_parts(rest) {
                k.fulltext.push((n, version));
            }
        }
    }
    k
}

/// A Databend index from `system.indexes`: its table and definition.
/// Inverted / ngram indexes read `table(col, …)k='v' …`; aggregating ones
/// are a query (`SELECT … FROM db.table`), kept in `options["query"]`.
pub(crate) fn databend_index(db: &str, name: &str, kind: &str, original: &str, definition: &str) -> Option<(String, IndexDef)> {
    let kind = kind.trim().to_ascii_uppercase();
    let own = |t: &str| -> Option<String> {
        let t = t.trim().trim_matches('`');
        match t.rsplit_once('.') {
            Some((d, n)) if d.trim_matches('`') == db => Some(n.trim_matches('`').to_string()),
            Some(_) => None,
            None => Some(t.to_string()),
        }
    };
    if kind == "AGGREGATING" {
        let query = if original.trim().is_empty() { definition } else { original }.trim().trim_end_matches(';').to_string();
        let upper = definition.to_ascii_uppercase();
        let from = upper.rfind(" FROM ")?;
        let table = own(definition[from + 6..].split_whitespace().next()?)?;
        let options = [("query".to_string(), query)].into();
        return Some((table, IndexDef { name: name.to_string(), kind: Some(kind), options, ..Default::default() }));
    }
    let open = definition.find('(')?;
    let table = own(&definition[..open])?;
    let columns = split_top(inside(definition, open)?).into_iter().map(|c| unquote(&c)).collect();
    Some((table, IndexDef { name: name.to_string(), columns, kind: Some(kind), options: manticore_settings(definition), ..Default::default() }))
}

/// OceanBase's sequence (oceanbase.DBA_OB_SEQUENCE_OBJECTS) as CREATE SEQUENCE.
/// `values`: start, increment, min, max, cache.
pub(crate) fn oceanbase_sequence(name: &str, values: [&str; 5], cycle: bool, order: bool) -> String {
    let [start, increment, min, max, cache] = values;
    let cache = match cache.trim() {
        "" | "0" | "1" => "NOCACHE".to_string(),
        c => format!("CACHE {c}"),
    };
    format!(
        "CREATE SEQUENCE {} START WITH {start} INCREMENT BY {increment} MINVALUE {min} MAXVALUE {max} {cache} {} {}",
        quote(name),
        if cycle { "CYCLE" } else { "NOCYCLE" },
        if order { "ORDER" } else { "NOORDER" }
    )
}

/// Databend's sequence (SHOW SEQUENCES) as CREATE SEQUENCE.
pub(crate) fn databend_sequence(name: &str, start: Option<&str>, increment: Option<&str>, comment: Option<&str>) -> String {
    let mut s = format!("CREATE SEQUENCE {}", quote(name));
    if let Some(v) = start.filter(|v| !v.is_empty()) {
        s.push_str(&format!(" START = {v}"));
    }
    if let Some(v) = increment.filter(|v| !v.is_empty()) {
        s.push_str(&format!(" INCREMENT = {v}"));
    }
    if let Some(c) = comment.filter(|c| !c.is_empty()) {
        s.push_str(&format!(" COMMENT = '{}'", c.replace('\'', "''")));
    }
    s
}

/// StarRocks / Doris rollups from `DESC t ALL`: (rollup, its columns). The
/// base index (named like the table) and synchronous materialized views
/// (columns that aren't the table's, like `mv_sum_b`) are left out.
pub(crate) fn olap_rollups(table: &str, columns: &[String], rows: &[(String, String)]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (index, field) in rows {
        if !index.is_empty() {
            out.push((index.clone(), Vec::new()));
        }
        if let Some(last) = out.last_mut().filter(|_| !field.is_empty()) {
            last.1.push(field.clone());
        }
    }
    out.retain(|(n, cols)| n != table && !cols.is_empty() && cols.iter().all(|c| columns.contains(c)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_with_spaces_and_quotes_in_the_name() {
        let tidb = "CREATE TABLE `t` (\n  `saldo` int,\n  CONSTRAINT `chk_cli``ente ñ_saldo` CHECK ((`saldo` >= 0)),\n  CONSTRAINT `chk_simple` CHECK ((`saldo` < 10)) /*T![check_constraint] NOT ENFORCED */\n) ENGINE=InnoDB";
        let c = create_checks(tidb);
        assert_eq!(c.len(), 2, "{c:?}");
        assert_eq!(c[0].name.as_deref(), Some("chk_cli`ente ñ_saldo"));
        assert_eq!(c[0].expression, "(`saldo` >= 0)");
        assert_eq!(c[1].name.as_deref(), Some("chk_simple"));
    }

    #[test]
    fn checks_ignore_lines_inside_literals() {
        // A generated column's literal with a raw newline and what looks like
        // a CHECK after it: not a constraint of the table.
        let tidb = "CREATE TABLE `adv5` (\n  `a` varchar(20) DEFAULT NULL,\n  `g` varchar(60) GENERATED ALWAYS AS (concat(`a`, _utf8mb4'x\n  CONSTRAINT `z` CHECK (1)')) VIRTUAL,\n  `h` varchar(9) DEFAULT 'it\\'s\n  CONSTRAINT `w` CHECK (1)',\n  CONSTRAINT `ck_nl` CHECK ((`a` != _utf8mb4'q,\nr'))\n) ENGINE=InnoDB";
        let c = create_checks(tidb);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].name.as_deref(), Some("ck_nl"));
        assert_eq!(c[0].expression, "(`a` != _utf8mb4'q,\nr')");
    }

    #[test]
    fn splits_and_key_values() {
        assert_eq!(split_top("a, (lower(`b`), 1), 'x,y'"), ["a", "(lower(`b`), 1)", "'x,y'"]);
        let kv = key_values("(\"bloom_filter_fpp\" = \"0.05\", \"gram_num\" = \"4\")");
        assert_eq!(kv.get("gram_num").map(String::as_str), Some("4"));
        assert_eq!(key_values("analyzer = 'English', case_sensitive = 'false'").get("analyzer").map(String::as_str), Some("English"));
        assert_eq!(olap_index_type("NGRAMBF(\"gram_num\" = \"4\")").0.as_deref(), Some("NGRAMBF"));
        assert_eq!(olap_index_type("BITMAP"), (Some("BITMAP".into()), BTreeMap::new()));
        assert_eq!(column_set("b, `a`"), "a, b");
    }

    #[test]
    fn column_checks_split_off() {
        assert_eq!(split_column_check("int(11) CHECK (`q` < (10))"), ("int(11)", Some("CHECK (`q` < (10))")));
        assert_eq!(split_column_check("longtext"), ("longtext", None));
        assert_eq!(split_column_check("int CHECK (a) + 1"), ("int CHECK (a) + 1", None));
    }

    #[test]
    fn key_parts() {
        let mut ix = IndexDef { name: "ix".into(), columns: vec!["a(10)".into(), "(lower(`a`))".into(), "p".into()], ..Default::default() };
        ix.options.insert("desc".into(), "a(10), (lower(`a`))".into());
        let parts: Vec<String> = ix.columns.iter().map(|c| key_part(&ix, c)).collect();
        assert_eq!(parts, ["`a`(10) DESC", "(lower(`a`)) DESC", "`p`"]);
    }

    #[test]
    fn show_create_parts() {
        let mysql = "CREATE TABLE `t` (\n  `b` text,\n  FULLTEXT KEY `ft` (`b`) /*!50100 WITH PARSER `ngram` */ ,\n  FULLTEXT KEY `f2` (`b`),\n  CONSTRAINT `ck_p` CHECK ((`p` > 0)),\n  CONSTRAINT `t_chk_1` CHECK ((`c` <> _utf8mb4'x')) /*!80016 NOT ENFORCED */\n) ENGINE=InnoDB";
        assert_eq!(fulltext_parsers(mysql), [("ft".to_string(), "ngram".to_string())]);
        let checks = create_checks(mysql);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].expression, "(`p` > 0)");
        assert_eq!(checks[1].name.as_deref(), Some("t_chk_1"));
        assert_eq!(checks[1].expression, "(`c` <> _utf8mb4'x') NOT ENFORCED");
        let tidb = "CONSTRAINT `c` CHECK (`a` > 1 AND `b` < 2)";
        assert_eq!(create_checks(tidb)[0].expression, "(`a` > 1 AND `b` < 2)");

        let greptime = "CREATE TABLE IF NOT EXISTS `m` (\n  `ts` TIMESTAMP(3) NOT NULL,\n  `host` STRING NULL SKIPPING INDEX WITH(granularity = '1024', type = 'BLOOM') INVERTED INDEX,\n  `msg` STRING NULL FULLTEXT INDEX WITH(analyzer = 'English', case_sensitive = 'false'),\n  TIME INDEX (`ts`)\n)";
        let g = greptime_indexes(greptime);
        assert_eq!(g.len(), 3);
        assert_eq!((g[0].0.as_str(), g[0].1.as_str(), g[0].2.get("type").map(String::as_str)), ("host", "INVERTED", None));
        let sk = g.iter().find(|x| x.1 == "SKIPPING").unwrap();
        assert_eq!(sk.2.get("granularity").map(String::as_str), Some("1024"));
        let ft = g.iter().find(|x| x.1 == "FULLTEXT").unwrap();
        assert_eq!((ft.0.as_str(), ft.2.get("analyzer").map(String::as_str)), ("msg", Some("English")));

        let sr = "CREATE TABLE `t` (\n  `id` int(11) NOT NULL COMMENT \"\"\n) ENGINE=OLAP \nDUPLICATE KEY(`id`)\nPROPERTIES (\n\"bloom_filter_columns\" = \"p, a\",\n\"replication_num\" = \"1\"\n);";
        assert_eq!(olap_properties(sr).get("bloom_filter_columns").map(String::as_str), Some("p, a"));

        let mc = "CREATE TABLE pr (\nid bigint,\ntitle text\n) min_infix_len='3' morphology='stem_en' blend_chars='\\'x'";
        let s = manticore_settings(mc);
        assert_eq!(s.get("morphology").map(String::as_str), Some("stem_en"));
        assert_eq!(s.get("blend_chars").map(String::as_str), Some("'x"));
        assert_eq!(manticore_setting("blend_chars", "'x"), "blend_chars='\\'x'");
    }

    #[test]
    fn greptime_table_options() {
        let c = "CREATE TABLE IF NOT EXISTS `w` (\n  `ts` TIMESTAMP(3) NOT NULL,\n  TIME INDEX (`ts`)\n)\n\nENGINE=mito\nWITH(\n  append_mode = 'true',\n  comment = 'hola',\n  'compaction.type' = 'twcs',\n  ttl = '7days'\n)";
        let w = greptime_with(c);
        assert_eq!(w.keys().map(String::as_str).collect::<Vec<_>>(), ["append_mode", "compaction.type", "ttl"]);
        assert_eq!(w["ttl"], "7days");
        assert!(greptime_with("CREATE TABLE `x` (\n  `ts` TIMESTAMP(3)\n)\nENGINE=mito").is_empty());
        assert_eq!((greptime_key("ttl"), greptime_key("compaction.type")), ("ttl".to_string(), "'compaction.type'".to_string()));
    }

    #[test]
    fn singlestore_show_create() {
        let c = "CREATE ROWSTORE TABLE `t` (\n  `a` int(11) DEFAULT NULL,\n  `b` int(11) DEFAULT NULL,\n  `t` text,\n  SORT KEY `s` (`a` DESC),\n  SHARD KEY `__SHARDKEY` (`a`),\n  KEY `b` (`b`) USING HASH,\n  UNIQUE KEY `u` (`a`) USING HASH,\n  FULLTEXT USING VERSION 2 `ft` (`t`)\n) AUTOSTATS_CARDINALITY_MODE=INCREMENTAL";
        let k = singlestore_keys(c);
        assert_eq!(k.table_type.as_deref(), Some("ROWSTORE"));
        assert_eq!(k.shard, Some(("__SHARDKEY".into(), "`a`".into())));
        assert_eq!(k.sort, Some(("s".into(), "`a` DESC".into())));
        assert_eq!(k.hash, ["b", "u"]);
        assert_eq!(k.fulltext, [("ft".to_string(), Some("2".to_string()))]);
        let old = singlestore_keys("CREATE TABLE `t` (\n  `a` int,\n  KEY `a` (`a`) USING CLUSTERED COLUMNSTORE,\n  FULLTEXT KEY `f` (`a`)\n)");
        assert_eq!((old.table_type, old.sort, old.fulltext), (None, Some(("a".into(), "`a`".into())), vec![("f".to_string(), None)]));
    }

    #[test]
    fn databend_indexes_and_sequences() {
        let (t, ix) = databend_index("db", "idx", "INVERTED", "", "docs(title, body)tokenizer='english' filters='english_stop'").unwrap();
        assert_eq!((t.as_str(), ix.columns.as_slice(), ix.kind.as_deref()), ("docs", &["title".to_string(), "body".into()][..], Some("INVERTED")));
        assert_eq!(ix.options.get("tokenizer").map(String::as_str), Some("english"));
        let (t, ix) = databend_index("db", "agg", "AGGREGATING", "SELECT MIN(a), MAX(c) FROM agg", "SELECT MAX(c), MIN(a) FROM db.agg").unwrap();
        assert_eq!((t.as_str(), ix.options["query"].as_str(), ix.columns.len()), ("agg", "SELECT MIN(a), MAX(c) FROM agg", 0));
        assert!(databend_index("db", "x", "INVERTED", "", "other.t(a)").is_none());
        assert_eq!(databend_sequence("s", Some("10"), Some("2"), Some("it's")), "CREATE SEQUENCE `s` START = 10 INCREMENT = 2 COMMENT = 'it''s'");
        assert_eq!(databend_sequence("s", None, None, None), "CREATE SEQUENCE `s`");
    }

    #[test]
    fn oceanbase_sequences() {
        assert_eq!(
            oceanbase_sequence("s", ["100", "5", "1", "999", "10"], true, false),
            "CREATE SEQUENCE `s` START WITH 100 INCREMENT BY 5 MINVALUE 1 MAXVALUE 999 CACHE 10 CYCLE NOORDER"
        );
        assert!(oceanbase_sequence("s", ["1", "1", "1", "9", "0"], false, true).contains("NOCACHE NOCYCLE ORDER"));
    }

    #[test]
    fn rollups_from_desc_all() {
        let row = |i: &str, f: &str| (i.to_string(), f.to_string());
        let rows = [row("r", "id"), row("", "a"), row("", "b"), row("", ""), row("r_a", "a"), row("", "b"), row("mv", "a"), row("", "mv_sum_b")];
        let cols = ["id".to_string(), "a".into(), "b".into()];
        assert_eq!(olap_rollups("r", &cols, &rows), [("r_a".to_string(), vec!["a".to_string(), "b".into()])]);
    }

    #[test]
    fn not_enforced_goes_after_the_condition() {
        let c = CheckDef { name: Some("c".into()), expression: "(`c` <> 'x') NOT ENFORCED".into() };
        let sql = "ALTER TABLE `t` ADD CONSTRAINT `c` CHECK ((`c` <> 'x') NOT ENFORCED);";
        assert_eq!(place_not_enforced(sql, &[c]), "ALTER TABLE `t` ADD CONSTRAINT `c` CHECK (`c` <> 'x') NOT ENFORCED;");
    }
}
