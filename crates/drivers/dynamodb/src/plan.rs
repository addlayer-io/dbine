//! Derived access plans for PartiQL statements.
//!
//! DynamoDB has no EXPLAIN: PartiQL is translated to one of the item APIs
//! and nothing reports which. The translation follows fixed rules on the
//! table's (or index's) key schema, though, so it can be derived from the
//! statement's WHERE clause, which is what DynamoDB tools show:
//! - every key attribute compared with `=` (on a table) → `GetItem`
//!   (`UpdateItem` / `DeleteItem` for writes);
//! - the partition key with `=` or `IN` → `Query`, with a sort key
//!   condition when there is one (`=`, `<`, `>`, `BETWEEN`,
//!   `begins_with`); the other conditions are a filter applied after
//!   reading;
//! - anything else (no partition key condition, or `OR` at the top level)
//!   → `Scan` of the whole table or index.

use dbine_driver::{Plan, PlanNode};

/// Key schema and size of the table (or index) a statement targets, from
/// DescribeTable.
#[derive(Debug, Clone, Default)]
pub(crate) struct Target {
    pub table: String,
    pub index: Option<String>,
    pub partition_key: String,
    pub sort_key: Option<String>,
    pub item_count: Option<i64>,
    pub size_bytes: Option<i64>,
    /// Billing mode, provisioned capacity…
    pub props: Vec<(String, String)>,
}

/// What a statement does, as far as plans care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    Select,
    Insert,
    Update,
    Delete,
    Other,
}

// ---- tokens ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// `"name"` or a bare name.
    Ident(String),
    /// A literal: string, number, `?`, list, map, `true`…
    Lit(String),
    /// A keyword, upper-cased.
    Kw(String),
    /// A function name (a word followed by `(`), lower-cased.
    Func(String),
    Op(String),
    Punct(char),
}

const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "BETWEEN", "IN", "IS", "MISSING", "NULL", "UPDATE", "DELETE", "INSERT",
    "INTO", "VALUE", "SET", "REMOVE", "RETURNING", "ORDER", "BY", "ASC", "DESC", "EXISTS",
];

fn tokens(s: &str) -> Vec<Tok> {
    let cs: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let quoted = |i: &mut usize, q: char| -> String {
        let mut v = String::new();
        *i += 1;
        while *i < cs.len() {
            if cs[*i] == q {
                if cs.get(*i + 1) == Some(&q) {
                    v.push(q);
                    *i += 2;
                    continue;
                }
                *i += 1;
                break;
            }
            v.push(cs[*i]);
            *i += 1;
        }
        v
    };
    while i < cs.len() {
        let c = cs[i];
        match c {
            _ if c.is_whitespace() => i += 1,
            '"' => out.push(Tok::Ident(quoted(&mut i, '"'))),
            '\'' => out.push(Tok::Lit(format!("'{}'", quoted(&mut i, '\'')))),
            '?' => {
                out.push(Tok::Lit("?".into()));
                i += 1;
            }
            '[' | '{' | '<' if c != '<' || cs.get(i + 1) == Some(&'<') => {
                // A list, map or bag literal: up to the matching close.
                let start = i;
                let mut depth = 0i32;
                let mut q: Option<char> = None;
                while i < cs.len() {
                    let d = cs[i];
                    if let Some(qc) = q {
                        if d == qc {
                            q = None;
                        }
                    } else {
                        match d {
                            '\'' | '"' => q = Some(d),
                            '[' | '{' => depth += 1,
                            '<' if cs.get(i + 1) == Some(&'<') => {
                                depth += 1;
                                i += 1;
                            }
                            ']' | '}' => depth -= 1,
                            '>' if cs.get(i + 1) == Some(&'>') => {
                                depth -= 1;
                                i += 1;
                            }
                            _ => {}
                        }
                    }
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                out.push(Tok::Lit(cs[start..i].iter().collect()));
            }
            '=' => {
                out.push(Tok::Op("=".into()));
                i += 1;
            }
            '<' | '>' | '!' => {
                let two: String = cs[i..(i + 2).min(cs.len())].iter().collect();
                if matches!(two.as_str(), "<=" | ">=" | "<>" | "!=") {
                    out.push(Tok::Op(two));
                    i += 2;
                } else {
                    out.push(Tok::Op(c.to_string()));
                    i += 1;
                }
            }
            '(' | ')' | ',' | '.' | ';' | '*' => {
                out.push(Tok::Punct(c));
                i += 1;
            }
            _ if c.is_ascii_digit() || (c == '-' && cs.get(i + 1).is_some_and(char::is_ascii_digit)) => {
                let start = i;
                i += 1;
                while i < cs.len() && (cs[i].is_ascii_alphanumeric() || matches!(cs[i], '.' | '+' | '-')) {
                    i += 1;
                }
                out.push(Tok::Lit(cs[start..i].iter().collect()));
            }
            _ if c.is_alphanumeric() || c == '_' => {
                let start = i;
                while i < cs.len() && (cs[i].is_alphanumeric() || matches!(cs[i], '_' | '-')) {
                    i += 1;
                }
                let w: String = cs[start..i].iter().collect();
                let up = w.to_ascii_uppercase();
                let mut j = i;
                while j < cs.len() && cs[j].is_whitespace() {
                    j += 1;
                }
                if cs.get(j) == Some(&'(') && !KEYWORDS.contains(&up.as_str()) {
                    out.push(Tok::Func(w.to_ascii_lowercase()));
                } else if matches!(up.as_str(), "TRUE" | "FALSE") {
                    out.push(Tok::Lit(up.to_ascii_lowercase()));
                } else if KEYWORDS.contains(&up.as_str()) {
                    out.push(Tok::Kw(up));
                } else {
                    out.push(Tok::Ident(w));
                }
            }
            _ => {
                out.push(Tok::Punct(c));
                i += 1;
            }
        }
    }
    out
}

fn show(ts: &[Tok]) -> String {
    let mut s = String::new();
    for t in ts {
        let piece = match t {
            Tok::Ident(n)
                if n.starts_with(|c: char| c.is_alphabetic() || c == '_')
                    && n.chars().all(|c| c.is_alphanumeric() || c == '_')
                    && !KEYWORDS.contains(&n.to_ascii_uppercase().as_str()) =>
            {
                n.clone()
            }
            Tok::Ident(n) => format!("\"{}\"", n.replace('"', "\"\"")),
            Tok::Lit(v) | Tok::Kw(v) | Tok::Func(v) | Tok::Op(v) => v.clone(),
            Tok::Punct(c) => c.to_string(),
        };
        let glue = matches!(t, Tok::Punct(')' | ',' | '.')) || s.ends_with('(') || s.ends_with('.') || s.is_empty();
        if !glue {
            s.push(' ');
        }
        s.push_str(&piece);
    }
    s
}

// ---- statement ---------------------------------------------------------

/// The statement's verb and the table / index it targets.
pub(crate) fn target_of(stmt: &str) -> (Verb, Option<(String, Option<String>)>) {
    let ts = tokens(stmt);
    let verb = match ts.first() {
        Some(Tok::Kw(k)) => match k.as_str() {
            "SELECT" => Verb::Select,
            "INSERT" => Verb::Insert,
            "UPDATE" => Verb::Update,
            "DELETE" => Verb::Delete,
            _ => Verb::Other,
        },
        _ => Verb::Other,
    };
    let after = match verb {
        Verb::Select | Verb::Delete => ts.iter().position(|t| *t == Tok::Kw("FROM".into())),
        Verb::Insert => ts.iter().position(|t| *t == Tok::Kw("INTO".into())),
        Verb::Update => Some(0),
        Verb::Other => None,
    };
    let target = after.and_then(|i| match (ts.get(i + 1), ts.get(i + 2), ts.get(i + 3)) {
        (Some(Tok::Ident(t)), Some(Tok::Punct('.')), Some(Tok::Ident(x))) => Some((t.clone(), Some(x.clone()))),
        (Some(Tok::Ident(t)), _, _) => Some((t.clone(), None)),
        _ => None,
    });
    (verb, target)
}

/// One top-level condition of the WHERE clause.
#[derive(Debug, Clone, PartialEq)]
enum Cond {
    Eq(String),
    In(String),
    /// `<`, `>`, `BETWEEN`, `begins_with`: usable on a sort key.
    Range(String),
    Other,
}

/// The WHERE clause's top-level conjuncts (text and kind) and whether it
/// has a top-level OR.
fn conditions(stmt: &str) -> (Vec<(String, Cond)>, bool) {
    let ts = tokens(stmt);
    let Some(w) = ts.iter().position(|t| *t == Tok::Kw("WHERE".into())) else { return (Vec::new(), false) };
    let mut depth = 0i32;
    let mut end = ts.len();
    for (i, t) in ts.iter().enumerate().skip(w + 1) {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth -= 1,
            Tok::Kw(k) if depth == 0 && matches!(k.as_str(), "RETURNING" | "ORDER") => {
                end = i;
                break;
            }
            Tok::Punct(';') => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let has_or = split_and(&ts[w + 1..end], &mut out);
    (out, has_or)
}

/// Splits on top-level ANDs (not BETWEEN's), unwrapping parenthesized
/// conjunctions; returns whether an OR showed up at the top level.
fn split_and(ts: &[Tok], out: &mut Vec<(String, Cond)>) -> bool {
    let (mut depth, mut start, mut between, mut has_or) = (0i32, 0, false, false);
    let mut parts = Vec::new();
    for (i, t) in ts.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth -= 1,
            Tok::Kw(k) if depth == 0 && k == "BETWEEN" => between = true,
            Tok::Kw(k) if depth == 0 && k == "AND" => {
                if between {
                    between = false;
                } else {
                    parts.push(&ts[start..i]);
                    start = i + 1;
                }
            }
            Tok::Kw(k) if depth == 0 && k == "OR" => has_or = true,
            _ => {}
        }
    }
    parts.push(&ts[start..]);
    if has_or {
        out.push((show(ts), Cond::Other));
        return true;
    }
    for p in parts.into_iter().filter(|p| !p.is_empty()) {
        let wrapped = p.first() == Some(&Tok::Punct('('))
            && p.last() == Some(&Tok::Punct(')'))
            && closes_at_end(p);
        if wrapped {
            if split_and(&p[1..p.len() - 1], out) {
                return true;
            }
        } else {
            out.push((show(p), classify_cond(p)));
        }
    }
    false
}

/// The `(` at the start matches the `)` at the end.
fn closes_at_end(p: &[Tok]) -> bool {
    let mut depth = 0i32;
    for (i, t) in p.iter().enumerate() {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => {
                depth -= 1;
                if depth == 0 {
                    return i == p.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

fn classify_cond(p: &[Tok]) -> Cond {
    let value = |t: &Tok| matches!(t, Tok::Lit(_));
    match p {
        [Tok::Ident(a), Tok::Op(o), v] | [v, Tok::Op(o), Tok::Ident(a)] if value(v) => match o.as_str() {
            "=" => Cond::Eq(a.clone()),
            "<" | "<=" | ">" | ">=" => Cond::Range(a.clone()),
            _ => Cond::Other,
        },
        [Tok::Ident(a), Tok::Kw(k), ..] if k == "BETWEEN" => Cond::Range(a.clone()),
        [Tok::Ident(a), Tok::Kw(k), rest @ ..] if k == "IN" && !rest.is_empty() => Cond::In(a.clone()),
        [Tok::Func(f), Tok::Punct('('), Tok::Ident(a), Tok::Punct(','), ..] if f == "begins_with" => Cond::Range(a.clone()),
        _ => Cond::Other,
    }
}

// ---- plan --------------------------------------------------------------

fn verb_name(v: Verb) -> &'static str {
    match v {
        Verb::Select => "SELECT",
        Verb::Insert => "INSERT",
        Verb::Update => "UPDATE",
        Verb::Delete => "DELETE",
        Verb::Other => "PartiQL",
    }
}

/// The derived plan of `stmt` on `t` (the statement's target): a node for
/// the statement, an optional `Filter`, and the item API it becomes.
pub(crate) fn derive(stmt: &str, verb: Verb, t: &Target) -> Plan {
    let object = match &t.index {
        Some(i) => format!("{}.{i}", t.table),
        None => t.table.clone(),
    };
    let on_index = t.index.is_some();
    let mut access = PlanNode { object: Some(object.clone()), ..Default::default() };
    access.props.push(("Clave de partición".into(), t.partition_key.clone()));
    if let Some(sk) = &t.sort_key {
        access.props.push(("Clave de ordenación".into(), sk.clone()));
    }
    if let Some(n) = t.item_count {
        access.props.push(("Ítems (aprox., cada 6 h)".into(), n.to_string()));
    }
    if let Some(b) = t.size_bytes {
        access.props.push(("Tamaño en bytes (aprox.)".into(), b.to_string()));
    }
    access.props.extend(t.props.iter().cloned());
    let mut filter = Vec::new();

    if verb == Verb::Insert {
        access.op = "PutItem".into();
        access.detail = "condicional: falla si el ítem ya existe".into();
        access.est_rows = Some(1.0);
    } else {
        let (conds, has_or) = conditions(stmt);
        let pk = &t.partition_key;
        let pk_cond = conds.iter().find(|(_, c)| matches!(c, Cond::Eq(a) | Cond::In(a) if a == pk));
        let sk_cond = t.sort_key.as_ref().and_then(|sk| {
            conds.iter().find(|(_, c)| matches!(c, Cond::Eq(a) | Cond::Range(a) if a == sk))
        });
        let full_key = matches!(pk_cond, Some((_, Cond::Eq(_))))
            && (t.sort_key.is_none() || matches!(sk_cond, Some((_, Cond::Eq(_)))));
        let used: Vec<&String> = pk_cond.iter().chain(sk_cond.iter()).map(|(s, _)| s).collect();
        let rest = || conds.iter().filter(|(s, _)| !used.contains(&s)).map(|(s, _)| s.clone()).collect::<Vec<_>>();
        let key_text = used.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" AND ");

        if has_or || pk_cond.is_none() {
            access.op = "Scan".into();
            access.detail = if on_index { "todo el índice".into() } else { "toda la tabla".into() };
            access.est_rows = t.item_count.map(|n| n as f64);
            access.warnings.push(format!("Scan: recorre {}", if on_index { "todo el índice" } else { "toda la tabla" }));
            filter = conds.iter().map(|(s, _)| s.clone()).collect();
            if !filter.is_empty() {
                access.warnings.push("El filtro se aplica después de leer: se consume capacidad por cada ítem leído".into());
            }
            if has_or {
                access.props.push(("Motivo".into(), "OR en el WHERE: no se puede usar una condición de clave".into()));
            } else {
                access.props.push(("Motivo".into(), format!("el WHERE no fija la clave de partición {pk} con = o IN")));
            }
            if matches!(verb, Verb::Update | Verb::Delete) {
                access.warnings.push(format!(
                    "{} de PartiQL necesita la clave primaria completa en el WHERE: DynamoDB lo rechazará",
                    verb_name(verb)
                ));
            }
        } else if full_key && !on_index {
            access.op = match verb {
                Verb::Update => "UpdateItem",
                Verb::Delete => "DeleteItem",
                _ => "GetItem",
            }
            .into();
            access.detail = key_text.clone();
            access.est_rows = Some(1.0);
            access.props.push(("Clave".into(), key_text));
            filter = rest();
            if matches!(verb, Verb::Update | Verb::Delete) && !filter.is_empty() {
                access.props.push(("Condición".into(), filter.join(" AND ")));
                filter.clear();
            }
        } else {
            access.op = "Query".into();
            access.detail = key_text.clone();
            access.props.push(("Condición de clave".into(), key_text));
            if matches!(pk_cond, Some((_, Cond::In(_)))) {
                access.props.push(("Nota".into(), "IN en la clave de partición: una consulta por valor".into()));
            }
            filter = rest();
            if matches!(verb, Verb::Update | Verb::Delete) {
                access.warnings.push(format!(
                    "{} de PartiQL necesita la clave primaria completa en el WHERE: DynamoDB lo rechazará",
                    verb_name(verb)
                ));
            }
        }
    }

    let mut root = PlanNode { op: verb_name(verb).into(), object: Some(object), ..Default::default() };
    root.props.push((
        "Origen".into(),
        "Derivado de la clave de la tabla: DynamoDB no expone planes de ejecución".into(),
    ));
    if filter.is_empty() {
        root.children.push(access);
    } else {
        let f = PlanNode {
            op: "Filter".into(),
            detail: filter.join(" AND "),
            props: vec![("Filtro".into(), filter.join(" AND "))],
            children: vec![access],
            ..Default::default()
        };
        root.children.push(f);
    }
    let raw = serde_json::json!({
        "table": t.table, "index": t.index, "partitionKey": t.partition_key, "sortKey": t.sort_key,
        "itemCount": t.item_count, "tableSizeBytes": t.size_bytes,
    });
    Plan {
        statement: stmt.to_string(),
        root,
        actual: false,
        raw_format: "json".into(),
        raw: serde_json::to_string_pretty(&raw).unwrap_or_default(),
    }
}

/// The node that reads (under the statement and the filter).
pub(crate) fn access_mut(p: &mut Plan) -> &mut PlanNode {
    let mut n = &mut p.root;
    while !n.children.is_empty() {
        n = &mut n.children[0];
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orders() -> Target {
        Target {
            table: "Orders".into(),
            partition_key: "customer".into(),
            sort_key: Some("order_id".into()),
            item_count: Some(12_000),
            ..Default::default()
        }
    }

    fn access(p: &Plan) -> &PlanNode {
        let mut n = &p.root;
        while !n.children.is_empty() {
            n = &n.children[0];
        }
        n
    }

    #[test]
    fn targets() {
        assert_eq!(target_of(r#"SELECT * FROM "Orders"."byDate" WHERE a = 1"#), (Verb::Select, Some(("Orders".into(), Some("byDate".into())))));
        assert_eq!(target_of("update Orders set x = 1 where customer = 'a'"), (Verb::Update, Some(("Orders".into(), None))));
        assert_eq!(target_of(r#"INSERT INTO "my-t" VALUE {'a': 1}"#), (Verb::Insert, Some(("my-t".into(), None))));
        assert_eq!(target_of(r#"DELETE FROM "t" WHERE "k" = 1"#).0, Verb::Delete);
        assert_eq!(target_of("EXISTS(SELECT * FROM t)").0, Verb::Other);
    }

    #[test]
    fn get_item_with_the_full_key() {
        let p = derive(r#"SELECT * FROM "Orders" WHERE "customer" = 'c1' AND order_id = 7"#, Verb::Select, &orders());
        let a = access(&p);
        assert_eq!(a.op, "GetItem");
        assert_eq!(a.est_rows, Some(1.0));
        assert!(a.warnings.is_empty());
        assert_eq!(p.root.children[0].op, "GetItem");
    }

    #[test]
    fn query_with_sort_key_range_and_filter() {
        let p = derive(
            "SELECT * FROM Orders WHERE customer = ? AND order_id BETWEEN 1 AND 9 AND status = 'open' ORDER BY order_id DESC",
            Verb::Select,
            &orders(),
        );
        let f = &p.root.children[0];
        assert_eq!(f.op, "Filter");
        assert_eq!(f.detail, "status = 'open'");
        let a = &f.children[0];
        assert_eq!(a.op, "Query");
        assert_eq!(a.detail, "customer = ? AND order_id BETWEEN 1 AND 9");
        assert!(a.warnings.is_empty());

        let p = derive(r#"SELECT * FROM Orders WHERE customer IN ['a', 'b'] AND begins_with("order_id", 'x')"#, Verb::Select, &orders());
        assert_eq!(access(&p).op, "Query");
        assert!(access(&p).props.iter().any(|(k, _)| k == "Nota"));
    }

    #[test]
    fn scan_without_partition_key_or_with_or() {
        let p = derive("SELECT * FROM Orders WHERE status = 'open'", Verb::Select, &orders());
        let a = access(&p);
        assert_eq!(a.op, "Scan");
        assert_eq!(a.est_rows, Some(12_000.0));
        assert!(a.warnings.iter().any(|w| w == "Scan: recorre toda la tabla"));
        assert!(a.warnings.iter().any(|w| w.starts_with("El filtro")));

        let p = derive("SELECT * FROM Orders WHERE customer = 'a' OR customer = 'b'", Verb::Select, &orders());
        assert_eq!(access(&p).op, "Scan");
        let p = derive("SELECT * FROM Orders", Verb::Select, &orders());
        assert_eq!(p.root.children[0].op, "Scan");
        assert_eq!(access(&p).warnings.len(), 1);
        // Parentheses around a conjunction don't hide the key.
        let p = derive("SELECT * FROM Orders WHERE (customer = 'a' AND (order_id = 1))", Verb::Select, &orders());
        assert_eq!(access(&p).op, "GetItem");
    }

    #[test]
    fn index_queries_and_writes() {
        let mut t = orders();
        t.index = Some("byStatus".into());
        t.partition_key = "status".into();
        t.sort_key = None;
        let p = derive(r#"SELECT * FROM "Orders"."byStatus" WHERE status = 'open'"#, Verb::Select, &t);
        assert_eq!((access(&p).op.as_str(), access(&p).object.as_deref()), ("Query", Some("Orders.byStatus")));

        let p = derive("UPDATE Orders SET s = 1 WHERE customer = 'a' AND order_id = 2 AND s = 0", Verb::Update, &orders());
        let a = access(&p);
        assert_eq!(a.op, "UpdateItem");
        assert!(a.props.iter().any(|(k, v)| k == "Condición" && v == "s = 0"));
        let p = derive("DELETE FROM Orders WHERE customer = 'a'", Verb::Delete, &orders());
        assert!(access(&p).warnings.iter().any(|w| w.contains("clave primaria completa")));
        let p = derive(r#"INSERT INTO Orders VALUE {'customer': 'a', 'order_id': 1}"#, Verb::Insert, &orders());
        assert_eq!(access(&p).op, "PutItem");
    }

    #[test]
    fn strings_with_keywords_are_values() {
        let p = derive("SELECT * FROM Orders WHERE customer = 'x OR y' AND order_id = 1", Verb::Select, &orders());
        assert_eq!(access(&p).op, "GetItem");
    }
}
