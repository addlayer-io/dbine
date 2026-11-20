//! Which Cypher reads may run without asking on Neo4j (docs/mcp.md,
//! "Reads"): the server runs them in READ access mode, which refuses any
//! write, but a procedure, a plugin's function (APOC's, GDS'…) or `LOAD
//! CSV` can act outside the graph (files, the network, other databases).
//! So, as for SQL, an allowlist: every function the statement calls has to
//! be one of Neo4j's own side-effect-free built-ins, and a procedure one of
//! its index searches. A plugin's function always has a namespace (Neo4j
//! refuses user functions in the root namespace), so an unqualified name is
//! a built-in's; a namespaced one passes only when listed. Anything else (a
//! name in backticks, a procedure not listed, `LOAD CSV`, a statement the
//! scan can't read to its end) goes to the user's approval.

/// Neo4j's own functions (Cypher 5 and 25), lowercase: aggregates,
/// predicates, scalar, list, math, string, temporal and spatial functions,
/// and Cypher 25's vector functions.
const FUNCTIONS: &[&str] = &[
    // Aggregates.
    "avg",
    "collect",
    "count",
    "max",
    "min",
    "percentilecont",
    "percentiledisc",
    "stdev",
    "stdevp",
    "sum",
    // Predicates.
    "all",
    "any",
    "exists",
    "isempty",
    "none",
    "single",
    // Scalar.
    "char_length",
    "character_length",
    "coalesce",
    "elementid",
    "endnode",
    "head",
    "id",
    "last",
    "length",
    "nullif",
    "properties",
    "randomuuid",
    "size",
    "startnode",
    "timestamp",
    "toboolean",
    "tobooleanornull",
    "tofloat",
    "tofloatornull",
    "tointeger",
    "tointegerornull",
    "type",
    "valuetype",
    // Lists and paths.
    "keys",
    "labels",
    "nodes",
    "range",
    "reduce",
    "relationships",
    "reverse",
    "tail",
    "tobooleanlist",
    "tofloatlist",
    "tointegerlist",
    "tostringlist",
    "shortestpath",
    "allshortestpaths",
    // Math.
    "abs",
    "ceil",
    "floor",
    "isnan",
    "rand",
    "round",
    "sign",
    "e",
    "exp",
    "log",
    "log10",
    "sqrt",
    "acos",
    "asin",
    "atan",
    "atan2",
    "cos",
    "cot",
    "degrees",
    "haversin",
    "pi",
    "radians",
    "sin",
    "tan",
    // Strings.
    "btrim",
    "left",
    "lower",
    "ltrim",
    "normalize",
    "replace",
    "right",
    "rtrim",
    "split",
    "substring",
    "tolower",
    "tostring",
    "tostringornull",
    "toupper",
    "trim",
    "upper",
    // Temporal and spatial values.
    "date",
    "datetime",
    "localdatetime",
    "localtime",
    "time",
    "duration",
    "point",
    // Vectors (Cypher 25, Neo4j 2025.10+).
    "vector",
    "vector_dimension_count",
    "vector_distance",
    "vector_norm",
];

/// Neo4j's own namespaced functions, lowercase.
const QUALIFIED_FUNCTIONS: &[&str] = &[
    "vector.similarity.cosine",
    "vector.similarity.euclidean",
    "point.distance",
    "point.withinbbox",
    "date.realtime",
    "date.statement",
    "date.transaction",
    "date.truncate",
    "datetime.fromepoch",
    "datetime.fromepochmillis",
    "datetime.realtime",
    "datetime.statement",
    "datetime.transaction",
    "datetime.truncate",
    "localdatetime.realtime",
    "localdatetime.statement",
    "localdatetime.transaction",
    "localdatetime.truncate",
    "localtime.realtime",
    "localtime.statement",
    "localtime.transaction",
    "localtime.truncate",
    "time.realtime",
    "time.statement",
    "time.transaction",
    "time.truncate",
    "duration.between",
    "duration.indays",
    "duration.inmonths",
    "duration.inseconds",
];

/// Neo4j's own procedures that only search an index, lowercase.
const PROCEDURES: &[&str] = &[
    "db.index.vector.querynodes",
    "db.index.vector.queryrelationships",
    "db.index.fulltext.querynodes",
    "db.index.fulltext.queryrelationships",
];

/// Words a `(` may follow without being a call: clauses, operators and
/// path keywords (`MATCH (n)`, `WHERE (a)`, `IN (…)`, `ANY SHORTEST (a)`).
const KEYWORDS: &[&str] = &[
    // `CALL (a, b) { … }`: a subquery's variables, not a procedure.
    "call",
    "match",
    "optional",
    "where",
    "and",
    "or",
    "xor",
    "not",
    "return",
    "with",
    "unwind",
    "in",
    "as",
    "case",
    "when",
    "then",
    "else",
    "distinct",
    "by",
    "skip",
    "limit",
    "offset",
    "is",
    "contains",
    "starts",
    "ends",
    "yield",
    "union",
    "shortest",
    "groups",
    "path",
    "paths",
    "walk",
    "trail",
    "acyclic",
    "repeatable",
    "elements",
    "different",
    "use",
    "filter",
    "let",
    "finish",
    "next",
    "asc",
    "desc",
    "ascending",
    "descending",
    "for",
    // A write's clauses (`CREATE (n)`, `MERGE (n)`, `FOREACH (x IN …)`):
    // the enforced read refuses the statement without asking anyone.
    "create",
    "merge",
    "foreach",
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok<'a> {
    /// An unquoted name or keyword.
    Word(&'a str),
    /// A name in backticks.
    Quoted,
    /// A string, number or `$parameter`.
    Value,
    Punct(char),
}

/// A character that ends a `//` comment, exactly as Neo4j's grammar has it
/// (`'//' ~[\r\n]*`): LF or CR. Ending it anywhere else would make the guard
/// and the server disagree about what is code, which either way can hide a
/// call; the Unicode line breaks, which a reader could take for one, are
/// refused instead (see `ambiguous_break`).
fn line_break(c: char) -> bool {
    matches!(c, '\n' | '\r')
}

/// A Unicode line break (NEL, LS, PS): Neo4j keeps it inside a `//`
/// comment but other tools end the line there. A query with one is
/// refused rather than guessed.
fn ambiguous_break(q: &str) -> Option<char> {
    q.chars()
        .find(|c| matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}'))
}

/// The tokens of a Cypher statement, comments left out. `Err` when a
/// string, quoted name or comment doesn't end.
fn tokens(q: &str) -> Result<Vec<Tok<'_>>, String> {
    let b = q.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    while i < q.len() {
        let c = q[i..].chars().next().unwrap_or(' ');
        if c.is_whitespace() {
            i += c.len_utf8();
        } else if b[i..].starts_with(b"//") {
            // Neo4j ends a `//` comment at CR as well as LF.
            i = q[i..].find(line_break).map_or(q.len(), |n| i + n);
        } else if b[i..].starts_with(b"/*") {
            i = i + 2 + q[i + 2..].find("*/").ok_or("/*")? + 2;
        } else if c == '\'' || c == '"' {
            let mut j = i + 1;
            loop {
                match b.get(j) {
                    None => return Err(c.to_string()),
                    Some(b'\\') => j += 2,
                    Some(&x) if x == c as u8 => break,
                    Some(_) => j += 1,
                }
            }
            out.push(Tok::Value);
            i = j + 1;
        } else if c == '`' {
            let mut j = i + 1;
            loop {
                match b.get(j) {
                    None => return Err("`".into()),
                    Some(b'`') if b.get(j + 1) == Some(&b'`') => j += 2,
                    Some(b'`') => break,
                    Some(_) => j += 1,
                }
            }
            out.push(Tok::Quoted);
            i = j + 1;
        } else if c == '$' && q[i + 1..].starts_with(|n: char| ident(n) || n == '`') {
            // `$name`, `$0` or `` $`a b` ``: a value.
            let rest = &q[i + 1..];
            if let Some(quoted) = rest.strip_prefix('`') {
                let end = quoted.find('`').ok_or("`")?;
                i += 1 + end + 2;
            } else {
                i += 1 + rest.find(|n: char| !ident(n)).unwrap_or(rest.len());
            }
            out.push(Tok::Value);
        } else if c.is_ascii_digit() {
            // `1`, `0x1F`, `1.5e-3`: not `1..3` (a slice's range).
            let mut j = i;
            loop {
                j += q[j..]
                    .find(|n: char| !(n.is_ascii_alphanumeric() || n == '_'))
                    .unwrap_or(q.len() - j);
                let exp = b[j - 1].eq_ignore_ascii_case(&b'e');
                if b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit)
                    || exp && matches!(b.get(j), Some(b'-' | b'+'))
                {
                    j += 1;
                } else {
                    break;
                }
            }
            out.push(Tok::Value);
            i = j;
        } else if ident(c) {
            let len = q[i..].find(|n: char| !ident(n)).unwrap_or(q.len() - i);
            out.push(Tok::Word(&q[i..i + len]));
            i += len;
        } else if c.is_ascii() && !c.is_control() {
            out.push(Tok::Punct(c));
            i += c.len_utf8();
        } else {
            // A character the server may read as a space, or as part of a
            // name (`apoc.x\u{1c}(…)`), where this scan wouldn't.
            return Err(format!("U+{:04X}", c as u32));
        }
    }
    Ok(out)
}

/// The dotted name ending at `end` (`db.index.vector.queryNodes`), lower
/// case, and where it starts; `Err` with the name when a part of it is in
/// backticks.
fn dotted(toks: &[Tok<'_>], end: usize) -> Result<(String, usize), String> {
    let mut from = end;
    while from >= 2 && toks[from - 1] == Tok::Punct('.') {
        match toks[from - 2] {
            Tok::Word(_) => from -= 2,
            Tok::Quoted => return Err("`…`".into()),
            _ => break,
        }
    }
    let name = toks[from..=end]
        .iter()
        .filter_map(|t| match t {
            Tok::Word(w) => Some(w.to_ascii_lowercase()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(".");
    Ok((name, from))
}

/// Whether a Cypher statement for `driver` calls nothing but listed
/// built-ins (`Err`: the first call that isn't, or why it wasn't read).
/// Only Neo4j's: its are the lists, and it's the Cypher engine that
/// enforces reads on the server.
pub(crate) fn approval_free(driver: &str, query: &str) -> Result<(), String> {
    if driver != "neo4j" {
        return Err(format!("{driver}: Cypher reads aren't checked"));
    }
    if let Some(c) = ambiguous_break(query) {
        return Err(format!("U+{:04X}", c as u32));
    }
    // Neo4j's older parser (4.4 to 5.26) turns `\uXXXX` into its character
    // before lexing, in comments, strings and code alike, so `//\u000a` or
    // `'\u0027` ends a comment or a string there and not here: refused.
    if query.contains("\\u") || query.contains("\\U") {
        return Err("\\u".into());
    }
    let toks = tokens(query)?;
    let word = |i: usize| match toks.get(i) {
        Some(Tok::Word(w)) => Some(w.to_ascii_lowercase()),
        _ => None,
    };
    // The last token of each procedure name (`CALL a.b.c`), with or
    // without its `(…)`.
    let mut procedures = Vec::new();
    for i in 0..toks.len() {
        match word(i).as_deref() {
            // Reads files or URLs from the server.
            Some("load") => return Err("LOAD CSV".into()),
            Some("call") => {}
            _ => continue,
        }
        match toks.get(i + 1) {
            // A subquery: `CALL { … }`, `CALL (a, b) { … }`.
            Some(Tok::Punct('{' | '(')) => continue,
            Some(Tok::Word(_)) => {}
            _ => return Err("CALL".into()),
        }
        let mut end = i + 1;
        while toks.get(end + 1) == Some(&Tok::Punct('.')) {
            match toks.get(end + 2) {
                Some(Tok::Word(_)) => end += 2,
                _ => return Err("CALL".into()),
            }
        }
        let (name, _) = dotted(&toks, end)?;
        if !PROCEDURES.contains(&name.as_str()) {
            return Err(name);
        }
        procedures.push(end);
    }
    for i in 1..toks.len() {
        if toks[i] != Tok::Punct('(') {
            continue;
        }
        match toks[i - 1] {
            Tok::Quoted => return Err("`…`".into()),
            Tok::Word(_) if procedures.contains(&(i - 1)) => {}
            Tok::Word(w) => {
                let (name, from) = dotted(&toks, i - 1)?;
                if from == i - 1 {
                    let w = w.to_ascii_lowercase();
                    if KEYWORDS.contains(&w.as_str()) || FUNCTIONS.contains(&w.as_str()) {
                        continue;
                    }
                    return Err(w);
                }
                if !QUALIFIED_FUNCTIONS.contains(&name.as_str()) {
                    return Err(name);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::approval_free;

    /// Similarity searches on Neo4j 2025 (Cypher 25's functions, and its
    /// `CYPHER 25` prefix) and Neo4j 5 (the `vector.similarity.*`
    /// functions, the vector index procedures) skip the approval; so do
    /// ordinary reads that call Neo4j's own functions.
    #[test]
    fn mcp_cypher_reads_with_built_ins_are_approval_free() {
        for q in [
            "CYPHER 25 MATCH (n:Doc) RETURN vector_distance(n.e, vector([1, 0, 0], 3, FLOAT32), COSINE) AS d, vector_norm(n.e, EUCLIDEAN) AS l, vector_dimension_count(n.e) AS n ORDER BY d LIMIT 5",
            "cypher 25 match (n:Doc {id: 2}) return VECTOR_DISTANCE(n.e, vector($q, 3, FLOAT32), EUCLIDEAN)",
            "MATCH (n:Doc) WITH n, vector.similarity.cosine(n.e, $q) AS s RETURN n.id, s ORDER BY s DESC LIMIT 5",
            "CYPHER 25 CALL db.index.vector.queryNodes('docs', 5, vector([4, 5, 6], 3, FLOAT32)) YIELD node, score RETURN node.id AS id, score",
            "CALL db.index.vector.queryRelationships('rels', 3, $q) YIELD relationship, score RETURN score",
            "MATCH (a:Person)-[r:KNOWS]->(b) WHERE NOT (b)-->(a) AND size(labels(b)) > 0 RETURN a.name, count(*) AS n, collect(DISTINCT b.name)[..3]",
            "MATCH p = shortestPath((a {id: 1})-[*..4]-(b {id: 2})) RETURN length(p), [x IN nodes(p) | x.name], reduce(t = 0, r IN relationships(p) | t + r.w)",
            "MATCH (n) WHERE n.name STARTS WITH 'a(' AND n.at > datetime.truncate('day', datetime()) RETURN toUpper(n.name), point.distance(n.loc, point({x: 1, y: 2}))",
            "MATCH (n) // apoc.do.it(n)\nRETURN n /* LOAD CSV apoc.x() */ LIMIT 1",
            "MATCH (n) CALL (n) { MATCH (n)-->(m) RETURN count(m) AS c } RETURN n, c",
            "MATCH (n) RETURN COUNT { (n)-->() } AS out, EXISTS { (n)<--() } AS has_in, n.score * (1 + rand())",
            "SHOW INDEXES YIELD name, type WHERE type = 'VECTOR' RETURN name",
            "MATCH (n:$($label)) RETURN n LIMIT 3",
            "UNWIND range(1, 10) AS i RETURN i, 1.5e-3 * (i), 0x1F",
            // A write goes to the enforced read too, which refuses it
            // (nobody is asked to approve a write as a read).
            "CREATE (:Doc {id: 1})",
            "MATCH (a) MERGE (a)-[:R]->(b:B) FOREACH (x IN [1] | SET a.x = x)",
        ] {
            assert_eq!(approval_free("neo4j", q), Ok(()), "{q}");
        }
    }

    /// Plugins' functions and procedures, procedures that aren't index
    /// searches, `LOAD CSV`, names in backticks and text the scan can't
    /// read ask first; so does every Cypher engine but Neo4j.
    #[test]
    fn mcp_cypher_reads_calling_anything_else_ask() {
        for (q, why) in [
            (
                "MATCH (n) RETURN apoc.text.join([n.a], ',')",
                "apoc.text.join",
            ),
            ("RETURN apoc.util.sleep(1000)", "apoc.util.sleep"),
            (
                "CALL apoc.load.json('http://x') YIELD value RETURN value",
                "apoc.load.json",
            ),
            ("CALL db.labels() YIELD label RETURN label", "db.labels"),
            ("CALL db.labels YIELD label RETURN label", "db.labels"),
            (
                "CALL dbms.listConfig() YIELD name RETURN name",
                "dbms.listconfig",
            ),
            (
                "CALL db.index.vector.createNodeIndex('i', 'L', 'e', 3, 'cosine')",
                "db.index.vector.createnodeindex",
            ),
            (
                "LOAD CSV FROM 'file:///etc/passwd' AS l RETURN l",
                "LOAD CSV",
            ),
            ("MATCH (n) RETURN `apoc`.x(n)", "`…`"),
            ("MATCH (n) RETURN `toUpper`(n.a)", "`…`"),
            (
                "MATCH (n) RETURN gds.similarity.cosine(n.a, n.b)",
                "gds.similarity.cosine",
            ),
            ("MATCH (n) RETURN graph.names()", "graph.names"),
            ("RETURN myfunc(1)", "myfunc"),
            ("MATCH (a)--(b) RETURN apoc.x(b)", "apoc.x"),
            ("MATCH (n) RETURN 'unterminated", "'"),
            ("MATCH (n) /* open", "/*"),
            ("USE graph.byName('x') MATCH (n) RETURN n", "graph.byname"),
            ("RETURN apoc.x\u{1c}(1)", "U+001C"),
            ("RETURN apoc.x\u{200b}(1)", "U+200B"),
        ] {
            assert_eq!(approval_free("neo4j", q), Err(why.to_string()), "{q}");
        }
        let q = "MATCH (n) RETURN n LIMIT 1";
        assert!(approval_free("memgraph", q).is_err());
        assert!(approval_free("neptune", q).is_err());
    }

    /// The guard ends a `//` comment where Neo4j does (CR or LF), so code
    /// after `//…\r` is code to both (it once hid LOAD CSV and procedure
    /// calls). The Unicode line breaks are refused: Neo4j keeps them inside
    /// the comment, so text after one must not be read as code either.
    #[test]
    fn a_comment_ends_where_neo4j_ends_it() {
        for q in [
            "WITH 1 AS x //\rLOAD CSV FROM 'http://x' AS l\nRETURN l",
            "MATCH (n) //\rCALL apoc.load.json('http://x') YIELD value\nRETURN n",
            "RETURN 1 //\u{2028}CALL dbms.components() YIELD name RETURN name",
            "RETURN 1 //\u{85}LOAD CSV FROM 'x' AS l RETURN l",
            // A block comment or a quote opened after the Unicode break would
            // have hidden the line the server runs.
            "WITH 1 AS x //\u{2028}/*\nLOAD CSV FROM 'http://x/' AS l RETURN l //*/",
            "WITH 1 AS x //\u{2029}'\nLOAD CSV FROM \"http://x/\" AS l RETURN l //'",
        ] {
            assert!(approval_free("neo4j", q).is_err(), "{q:?}");
        }
        assert_eq!(
            approval_free("neo4j", "RETURN 1 //\u{2028}x"),
            Err("U+2028".to_string())
        );
        // `\uXXXX` written as six characters: Neo4j's older parser reads it
        // as the character itself before lexing.
        for q in [
            r"WITH 1 AS x //\u000aLOAD CSV FROM 'http://x/' AS l RETURN l",
            r#"WITH '\u0027 AS x LOAD CSV FROM "http://x/" AS l RETURN l //'"#,
            r"MATCH (n) /* \u002a\u002f CALL apoc.x() YIELD v */ RETURN n",
            r"RETURN '\U0027' AS x",
        ] {
            assert_eq!(approval_free("neo4j", q), Err("\\u".to_string()), "{q}");
        }
        // CRLF line ends still read as plain comments.
        assert_eq!(
            approval_free("neo4j", "MATCH (n) // first\r\nRETURN n LIMIT 1"),
            Ok(())
        );
    }
}
