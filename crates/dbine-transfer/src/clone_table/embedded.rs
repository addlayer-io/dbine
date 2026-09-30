//! What a clone does differently on SQLite, libSQL, DuckDB and Firebird.
//!
//! - SQLite, libSQL and DuckDB keep each table's `CREATE` statement as it
//!   was written (`sqlite_master.sql`, `duckdb_tables().sql`), while the
//!   catalog they report (and `database_schema`) leaves out column
//!   collations (`COLLATE NOCASE`), computed columns, named table
//!   constraints and the like. The clone is created from that statement
//!   with only its names changed ([`native_ddl`]): the table's, a reference
//!   to the table itself, the explicit indexes' and, in DuckDB, the
//!   sequences its defaults draw from (the clone gets its own, where the
//!   original's is, so inserting into the clone never moves the
//!   original's). A statement it can't read is refused, never guessed.
//! - SQLite's AUTOINCREMENT counter (`sqlite_sequence`) and Firebird's
//!   identity generator end where the original's are, not after the
//!   largest copied id ([`copy_sqlite_sequence`], [`firebird_identity`]).
//! - Firebird 4 and later take names of 63 characters
//!   ([`firebird_generated_limit`]).

use super::{exec, scalar, strings, rename_constraint, ClonePlan, Ddl, Rename};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{DbObject, Driver, Error, Result, Session, TableSchema};

/// A string for inside a SQL literal (`'…'`).
fn lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Same name ignoring case (SQLite, DuckDB).
fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// `"schema".` before SQLite's catalog tables (`sqlite_master`,
/// `sqlite_sequence`) of an attached database.
fn sqlite_prefix(schema: Option<&str>) -> String {
    schema.filter(|s| !s.is_empty()).map(|s| format!("{}.", quote_ident(Quote::Double, s))).unwrap_or_default()
}

/// A name SQLite and libSQL refuse for a table: `sqlite_…` is theirs.
pub fn name_problem(info: &dbine_driver::DriverInfo, name: &str) -> Option<String> {
    (matches!(info.id, "sqlite" | "libsql") && name.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("sqlite_"))).then(|| {
        format!("{} no admite el nombre «{name}»: los nombres que empiezan con «sqlite_» son del motor; elegí otro nombre", info.name)
    })
}

// -- CREATE statements as the engine keeps them (SQLite, libSQL, DuckDB) ---------------------------

/// A token of a stored CREATE statement: enough to find the names in it.
#[derive(Debug, Clone, PartialEq)]
enum Tk {
    /// A keyword or an unquoted name, as written.
    Word(String),
    /// A quoted name, unquoted.
    Quoted(String),
    /// A string literal, unescaped.
    Str(String),
    Punct(char),
}

#[derive(Debug, Clone)]
struct Token {
    tk: Tk,
    start: usize,
    end: usize,
}

fn unparsed() -> Error {
    Error::Unsupported("no se puede clonar: DBine no pudo interpretar el CREATE que guarda el motor para esta tabla".into())
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// `sql`'s tokens (comments dropped). `brackets`: `[name]` is a quoted
/// name (SQLite), not a list (DuckDB).
fn tokens(sql: &str, brackets: bool) -> Result<Vec<Token>> {
    let mut out = Vec::new();
    let mut it = sql.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let next = it.peek().map(|p| p.1);
        if c.is_whitespace() {
            continue;
        }
        if c == '-' && next == Some('-') {
            while it.next_if(|p| p.1 != '\n').is_some() {}
            continue;
        }
        if c == '/' && next == Some('*') {
            it.next();
            let mut prev = ' ';
            loop {
                let Some((_, d)) = it.next() else { return Err(unparsed()) };
                if prev == '*' && d == '/' {
                    break;
                }
                prev = d;
            }
            continue;
        }
        let close = match c {
            '\'' | '"' | '`' => Some(c),
            '[' if brackets => Some(']'),
            _ => None,
        };
        if let Some(q) = close {
            let mut v = String::new();
            let end = loop {
                let Some((j, d)) = it.next() else { return Err(unparsed()) };
                if d == q {
                    if q != ']' && it.next_if(|p| p.1 == q).is_some() {
                        v.push(q);
                        continue;
                    }
                    break j + 1;
                }
                v.push(d);
            };
            out.push(Token { tk: if c == '\'' { Tk::Str(v) } else { Tk::Quoted(v) }, start: i, end });
            continue;
        }
        if word_char(c) {
            let mut end = i + c.len_utf8();
            while let Some((j, d)) = it.next_if(|p| word_char(p.1)) {
                end = j + d.len_utf8();
            }
            out.push(Token { tk: Tk::Word(sql[i..end].to_string()), start: i, end });
            continue;
        }
        out.push(Token { tk: Tk::Punct(c), start: i, end: i + c.len_utf8() });
    }
    Ok(out)
}

fn is_word(t: Option<&Token>, w: &str) -> bool {
    matches!(t.map(|t| &t.tk), Some(Tk::Word(x)) if x.eq_ignore_ascii_case(w))
}

fn is_punct(t: Option<&Token>, c: char) -> bool {
    matches!(t.map(|t| &t.tk), Some(Tk::Punct(x)) if *x == c)
}

fn name_part(t: Option<&Token>) -> Option<String> {
    match &t?.tk {
        Tk::Word(w) | Tk::Quoted(w) => Some(w.clone()),
        _ => None,
    }
}

/// A dotted name starting at token `i`: its parts, and the token after it.
fn dotted(toks: &[Token], i: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = vec![name_part(toks.get(i))?];
    let mut j = i + 1;
    while is_punct(toks.get(j), '.') {
        parts.push(name_part(toks.get(j + 1))?);
        j += 2;
    }
    Some((parts, j))
}

/// Past `CREATE <words> <keyword> [IF NOT EXISTS]`: the token of the name.
fn after_keyword(toks: &[Token], keyword: &str) -> Result<usize> {
    if !is_word(toks.first(), "CREATE") {
        return Err(unparsed());
    }
    let mut i = 1;
    while !is_word(toks.get(i), keyword) {
        // TEMP, UNIQUE…
        if !matches!(toks.get(i).map(|t| &t.tk), Some(Tk::Word(_))) {
            return Err(unparsed());
        }
        i += 1;
    }
    i += 1;
    if is_word(toks.get(i), "IF") && is_word(toks.get(i + 1), "NOT") && is_word(toks.get(i + 2), "EXISTS") {
        i += 3;
    }
    Ok(i)
}

fn splice(sql: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(sql.len() + 64);
    let mut at = 0;
    for (start, end, text) in edits {
        out.push_str(&sql[at..start]);
        out.push_str(&text);
        at = end;
    }
    out.push_str(&sql[at..]);
    out
}

/// The table names a query's `FROM` / `JOIN` lists from token `from` on,
/// as token ranges: `FROM a [[AS] x], b …`, `JOIN c`. The `FROM` of
/// `EXTRACT(… FROM v)`, `SUBSTRING(… FROM n)` or `TRIM(… FROM v)` names no
/// table: only one with a `SELECT` before it in the same parentheses does.
fn table_refs(toks: &[Token], from: usize) -> Vec<(usize, usize)> {
    const CLAUSE: [&str; 21] = [
        "WHERE", "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL", "ON", "GROUP", "ORDER", "HAVING", "UNION", "ROWS", "FETCH", "OFFSET",
        "FIRST", "PLAN", "WINDOW", "USING", "LATERAL",
    ];
    let selected = |k: usize| {
        let mut depth = 0i32;
        for t in toks[from..k].iter().rev() {
            match &t.tk {
                Tk::Punct(')') => depth += 1,
                Tk::Punct('(') if depth == 0 => return false,
                Tk::Punct('(') => depth -= 1,
                Tk::Word(w) if depth == 0 && w.eq_ignore_ascii_case("SELECT") => return true,
                _ => {}
            }
        }
        false
    };
    let mut out = Vec::new();
    for k in from..toks.len() {
        if !(is_word(toks.get(k), "JOIN") || (is_word(toks.get(k), "FROM") && selected(k))) {
            continue;
        }
        let mut j = k + 1;
        while let Some((_, end)) = dotted(toks, j) {
            out.push((j, end));
            j = end;
            if is_word(toks.get(j), "AS") {
                j += 1;
            }
            match toks.get(j).map(|t| &t.tk) {
                Some(Tk::Word(w)) if !CLAUSE.iter().any(|c| w.eq_ignore_ascii_case(c)) => j += 1,
                Some(Tk::Quoted(_)) => j += 1,
                _ => {}
            }
            if !is_punct(toks.get(j), ',') {
                break;
            }
            j += 1;
        }
    }
    out
}

/// A column qualified with the table's own name (`t.v`, `"t".v`, the
/// `t` of `main.t.v`) in a CHECK, a computed column or an index
/// expression: the qualifier becomes `new_bare` (the clone's name), since
/// the clone's statements can't name the original's columns. Names after
/// `REFERENCES` are another matter and stay. A subquery that reads the
/// table itself (`FROM old`, `JOIN old`, `FROM a, old`: Firebird's CHECKs
/// and computed columns) reads the clone.
fn requalify_edits(toks: &[Token], from: usize, old: &str, new_bare: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let tables = table_refs(toks, from);
    let mut k = from;
    while k < toks.len() {
        if is_word(toks.get(k), "REFERENCES") {
            k = dotted(toks, k + 1).map(|(_, e)| e).unwrap_or(k + 1);
            continue;
        }
        // Past a table name of a FROM or JOIN (see `table_refs`).
        if let Some(&(_, end)) = tables.iter().find(|t| t.0 == k) {
            if toks.get(end - 1).is_some_and(|t| matches!(&t.tk, Tk::Word(n) | Tk::Quoted(n) if same_name(n, old))) {
                out.push((toks[end - 1].start, toks[end - 1].end, new_bare.to_string()));
            }
            k = end;
            continue;
        }
        // The part right before the column's: `old.col`, never `old.x.col`.
        if name_part(toks.get(k)).is_some_and(|n| same_name(&n, old))
            && is_punct(toks.get(k + 1), '.')
            && name_part(toks.get(k + 2)).is_some()
            && !is_punct(toks.get(k + 3), '.')
        {
            out.push((toks[k].start, toks[k].end, new_bare.to_string()));
            k += 3;
            continue;
        }
        k += 1;
    }
    out
}

/// [`requalify_edits`] on an expression the catalog reports (Firebird's
/// CHECKs, computed columns and index expressions); one it can't read
/// stays as it is.
pub(super) fn requalify(expr: &str, old: &str, new_bare: &str) -> String {
    match tokens(expr, false) {
        Ok(toks) => splice(expr, requalify_edits(&toks, 0, old, new_bare)),
        Err(_) => expr.to_string(),
    }
}

/// Whether `expr` (a catalog expression) reads the table `old` in a
/// subquery (`FROM old`, `JOIN old`).
pub(super) fn reads_table(expr: &str, old: &str) -> bool {
    let Ok(toks) = tokens(expr, false) else { return false };
    table_refs(&toks, 0).iter().any(|&(_, end)| matches!(&toks[end - 1].tk, Tk::Word(n) | Tk::Quoted(n) if same_name(n, old)))
}

/// Firebird: which of `names` an index or a constraint of the database
/// already has (both unique per database, whatever the table).
pub(super) fn firebird_taken_sql(names: &[String]) -> String {
    let list = names.iter().map(|n| format!("'{}'", lit(n))).collect::<Vec<_>>().join(", ");
    format!(
        "SELECT TRIM(RDB$INDEX_NAME) FROM RDB$INDICES WHERE RDB$INDEX_NAME IN ({list}) \
         UNION SELECT TRIM(RDB$CONSTRAINT_NAME) FROM RDB$RELATION_CONSTRAINTS WHERE RDB$CONSTRAINT_NAME IN ({list})"
    )
}

/// A stored `CREATE TABLE`, rewritten for the clone.
#[derive(Debug, Clone, PartialEq)]
struct CreateRewrite {
    sql: String,
    /// Columns the engine computes (`AS (…)`, `GENERATED ALWAYS AS (…)`).
    generated: Vec<String>,
    /// Every `nextval('…')` literal, as written (DuckDB's sequences).
    sequences: Vec<String>,
}

/// The original's `CREATE TABLE` (`sql`, as the engine keeps it) with the
/// clone's name (`new_qualified`), a reference to the table itself pointed
/// at the clone (`new_bare` when the reference has no schema) and each
/// `nextval('x')` in `sequences` (literal → new literal) replaced.
fn rewrite_create_table(
    sql: &str,
    brackets: bool,
    schema: Option<&str>,
    old: &str,
    new_qualified: &str,
    new_bare: &str,
    sequences: &[(String, String)],
) -> Result<CreateRewrite> {
    let toks = tokens(sql, brackets)?;
    let i = after_keyword(&toks, "TABLE")?;
    let (_, after) = dotted(&toks, i).ok_or_else(unparsed)?;
    if !is_punct(toks.get(after), '(') {
        return Err(unparsed());
    }
    let mut edits = vec![(toks[i].start, toks[after - 1].end, new_qualified.to_string())];
    edits.extend(requalify_edits(&toks, after, old, new_bare));
    let mut generated: Vec<String> = Vec::new();
    let mut seqs: Vec<String> = Vec::new();
    let mut depth = 0usize;
    // The current element's column (`None`: a table constraint).
    let mut column: Option<String> = None;
    let mut at_start = false;
    let mut closed = false;
    for k in after..toks.len() {
        let t = &toks[k];
        match t.tk {
            Tk::Punct('(') => {
                depth += 1;
                if depth == 1 {
                    at_start = true;
                    continue;
                }
            }
            Tk::Punct(')') => {
                depth = depth.checked_sub(1).ok_or_else(unparsed)?;
                if depth == 0 {
                    closed = true;
                    break;
                }
            }
            Tk::Punct(',') if depth == 1 => {
                at_start = true;
                continue;
            }
            _ => {}
        }
        if at_start {
            at_start = false;
            let constraint = ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"].iter().any(|w| is_word(Some(t), w));
            column = if constraint { None } else { name_part(Some(t)) };
            continue;
        }
        if depth == 1 && is_word(Some(t), "AS") && is_punct(toks.get(k + 1), '(') {
            if let Some(c) = column.as_ref().filter(|c| !generated.contains(c)) {
                generated.push(c.clone());
            }
        }
        if is_word(Some(t), "REFERENCES") {
            if let Some((parts, end)) = dotted(&toks, k + 1) {
                let n = parts.len();
                let same_schema = n == 1 || (n == 2 && schema.is_some_and(|s| same_name(s, &parts[0])));
                if same_schema && same_name(&parts[n - 1], old) {
                    edits.push((toks[k + 1].start, toks[end - 1].end, if n == 1 { new_bare } else { new_qualified }.to_string()));
                }
            }
        }
        if is_word(Some(t), "nextval") && is_punct(toks.get(k + 1), '(') {
            if let Some(Tk::Str(l)) = toks.get(k + 2).map(|t| &t.tk) {
                if !seqs.contains(l) {
                    seqs.push(l.clone());
                }
                if let Some((_, to)) = sequences.iter().find(|(from, _)| from == l) {
                    edits.push((toks[k + 2].start, toks[k + 2].end, format!("'{}'", lit(to))));
                }
            }
        }
    }
    if !closed {
        return Err(unparsed());
    }
    Ok(CreateRewrite { sql: splice(sql, edits), generated, sequences: seqs })
}

/// A stored `CREATE [UNIQUE] INDEX name ON table …` with both names
/// replaced, and the columns qualified with `old` (the original's name) in
/// its expressions and `WHERE` qualified with `bare` (the clone's).
fn rewrite_create_index(sql: &str, brackets: bool, index: &str, table: &str, old: &str, bare: &str) -> Result<String> {
    let toks = tokens(sql, brackets)?;
    let i = after_keyword(&toks, "INDEX")?;
    let (_, after) = dotted(&toks, i).ok_or_else(unparsed)?;
    if !is_word(toks.get(after), "ON") {
        return Err(unparsed());
    }
    let (_, end) = dotted(&toks, after + 1).ok_or_else(unparsed)?;
    let mut edits = vec![(toks[i].start, toks[after - 1].end, index.to_string()), (toks[after + 1].start, toks[end - 1].end, table.to_string())];
    edits.extend(requalify_edits(&toks, end, old, bare));
    Ok(splice(sql, edits))
}

/// The clone's DDL from the original's own `CREATE` statements, on engines
/// that keep them whole (SQLite, libSQL, DuckDB): the catalog they report
/// leaves out column collations, computed columns, named table constraints
/// and the like, which a clone from it would lose.
pub(super) struct Native {
    /// Run before the `CREATE TABLE` (DuckDB: the clone's own sequences),
    /// each with the statement that undoes it.
    before: Vec<(String, String)>,
    create: String,
    indexes: Vec<String>,
    /// Computed columns: never loaded.
    generated: Vec<String>,
    renames: Vec<Rename>,
    notes: Vec<String>,
}

pub(super) async fn native_ddl(
    driver: &dyn Driver,
    s: &mut dyn Session,
    t: &TableSchema,
    new_name: &str,
    objects: &[DbObject],
    with_indexes: bool,
) -> Result<Option<Native>> {
    let id = driver.info().id;
    let sqlite = matches!(id, "sqlite" | "libsql");
    if !sqlite && id != "duckdb" {
        return Ok(None);
    }
    let schema = t.schema.as_deref().filter(|s| !s.is_empty());
    // SQLite: tables and indexes share one namespace, and the explorer
    // lists no indexes: a name an index has is refused now.
    if sqlite {
        let p = sqlite_prefix(schema);
        let taken = strings(s, &format!("SELECT name FROM {p}sqlite_master WHERE type = 'index' AND lower(name) = lower('{}')", lit(new_name))).await?;
        if let Some(n) = taken.into_iter().next().and_then(|r| r.into_iter().next().flatten()) {
            return Err(Error::State(format!("ya existe un objeto llamado «{n}» (un índice); elegí otro nombre")));
        }
    }
    let clone_q = qualified_name(Quote::Double, schema, new_name);
    let clone_bare = quote_ident(Quote::Double, new_name);
    let (table_sql, index_sql) = if sqlite {
        let p = sqlite_prefix(schema);
        let n = lit(&t.name);
        (
            format!("SELECT sql FROM {p}sqlite_master WHERE type = 'table' AND name = '{n}'"),
            format!("SELECT name, sql FROM {p}sqlite_master WHERE type = 'index' AND tbl_name = '{n}' AND sql IS NOT NULL ORDER BY name"),
        )
    } else {
        let w = format!(
            "database_name = current_database() AND schema_name = '{}' AND table_name = '{}'",
            lit(schema.unwrap_or("main")),
            lit(&t.name)
        );
        (
            format!("SELECT sql FROM duckdb_tables() WHERE {w}"),
            format!("SELECT index_name, sql FROM duckdb_indexes() WHERE {w} AND sql IS NOT NULL ORDER BY index_name"),
        )
    };
    let create = strings(s, &table_sql)
        .await?
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next().flatten())
        .ok_or_else(|| Error::Unsupported("no se puede clonar: el motor no devolvió el CREATE de la tabla".into()))?;
    if tokens(&create, sqlite).ok().is_some_and(|k| is_word(k.get(1), "VIRTUAL")) {
        return Err(Error::Unsupported(
            "no se puede clonar: es una tabla virtual (FTS, R*Tree…), cuyo contenido administra un módulo del motor; DBine clona tablas comunes".into(),
        ));
    }
    let found = rewrite_create_table(&create, sqlite, schema, &t.name, &clone_q, &clone_bare, &[])?;

    // DuckDB: a `nextval` default names a sequence. The clone gets its own,
    // where the original's is, so its inserts never move the original's.
    let mut before = Vec::new();
    let mut renames: Vec<Rename> = Vec::new();
    let mut map = Vec::new();
    if !found.sequences.is_empty() {
        let all = strings(
            s,
            "SELECT schema_name, sequence_name, start_value::VARCHAR, min_value::VARCHAR, max_value::VARCHAR, increment_by::VARCHAR, cycle::VARCHAR, last_value::VARCHAR
             FROM duckdb_sequences() WHERE database_name = current_database()",
        )
        .await?;
        for literal in &found.sequences {
            let lt = tokens(literal, false)?;
            let parts = dotted(&lt, 0).filter(|(_, end)| *end == lt.len()).map(|(p, _)| p).ok_or_else(unparsed)?;
            let n = parts.len();
            let seq_schema = if n >= 2 { parts[n - 2].clone() } else { schema.unwrap_or("main").to_string() };
            let row = all
                .iter()
                .find(|r| {
                    r.first().cloned().flatten().is_some_and(|x| same_name(&x, &seq_schema))
                        && r.get(1).cloned().flatten().is_some_and(|x| same_name(&x, &parts[n - 1]))
                })
                .ok_or_else(|| Error::Unsupported(format!("no se puede clonar: no se encontró la secuencia «{literal}» que usa la tabla")))?;
            let num = |k: usize| row.get(k).cloned().flatten().and_then(|v| v.trim().parse::<i128>().ok());
            let (Some(first), Some(min), Some(max), Some(inc)) = (num(2), num(3), num(4), num(5)) else {
                return Err(unparsed());
            };
            let cycle = row.get(6).cloned().flatten().as_deref() == Some("true");
            let seq_name = row.get(1).cloned().flatten().unwrap_or_default();
            let mut start = num(7).map(|last| last + inc).unwrap_or(first);
            if start < min || start > max {
                if !cycle {
                    return Err(Error::Unsupported(format!(
                        "no se puede clonar: la secuencia «{seq_name}» del original llegó a su límite; el clon no podría seguirla"
                    )));
                }
                start = if inc > 0 { min } else { max };
            }
            let sch = row.first().cloned().flatten().unwrap_or_else(|| "main".into());
            // DuckDB's nextval('…') doesn't undo a doubled `"` inside a
            // quoted name: the clone's sequence gets none.
            let (base, _) = rename_constraint(&seq_name, &t.name, &new_name.replace('"', "_"), 0);
            let base = base.replace('"', "_");
            let mut new_seq = base.clone();
            let mut k = 1;
            while objects.iter().any(|o| same_name(&o.name, &new_seq) && o.schema.as_deref().is_none_or(|x| same_name(x, &sch)))
                || renames.iter().any(|r| same_name(&r.to, &new_seq))
            {
                k += 1;
                new_seq = format!("{base}_{k}");
            }
            let q = qualified_name(Quote::Double, Some(&sch), &new_seq);
            before.push((
                format!(
                    "CREATE SEQUENCE {q} INCREMENT BY {inc} MINVALUE {min} MAXVALUE {max} START WITH {start} {}CYCLE",
                    if cycle { "" } else { "NO " }
                ),
                format!("DROP SEQUENCE IF EXISTS {q}"),
            ));
            map.push((literal.clone(), q));
            renames.push(Rename { from: seq_name, to: new_seq, shortened: false });
        }
    }
    let table = rewrite_create_table(&create, sqlite, schema, &t.name, &clone_q, &clone_bare, &map)?;

    // Index names are the schema's, not the table's: one another object
    // already has is avoided now, not found out after the rows.
    let taken: Vec<String> = if !with_indexes {
        Vec::new()
    } else if sqlite {
        let p = sqlite_prefix(schema);
        strings(s, &format!("SELECT name FROM {p}sqlite_master WHERE type IN ('table', 'index', 'view') AND name IS NOT NULL")).await?
    } else {
        strings(
            s,
            &format!(
                "SELECT index_name FROM duckdb_indexes() WHERE database_name = current_database() AND schema_name = '{}'",
                lit(schema.unwrap_or("main"))
            ),
        )
        .await?
    }
    .into_iter()
    .filter_map(|r| r.into_iter().next().flatten())
    .chain([new_name.to_string()])
    .collect();
    let mut indexes = Vec::new();
    let mut left_out = Vec::new();
    let mut moved = Vec::new();
    for r in strings(s, &index_sql).await? {
        let mut r = r.into_iter();
        let (Some(name), Some(sql)) = (r.next().flatten(), r.next().flatten()) else { continue };
        if !with_indexes {
            let unique = tokens(&sql, sqlite).ok().is_some_and(|k| is_word(k.get(1), "UNIQUE"));
            left_out.push((name, unique));
            continue;
        }
        let (base, _) = rename_constraint(&name, &t.name, new_name, 0);
        let mut to = base.clone();
        let mut k = 1;
        while taken.iter().any(|x| same_name(x, &to)) || renames.iter().any(|r| same_name(&r.to, &to)) {
            k += 1;
            to = format!("{base}_{k}");
        }
        if to != base {
            moved.push(format!("el índice «{name}» del clon se llama «{to}»: «{base}» ya existe en el esquema"));
        }
        // SQLite: the index's name carries the schema, the table's can't;
        // DuckDB: the other way round.
        let (index, on) = if sqlite {
            (qualified_name(Quote::Double, schema, &to), clone_bare.clone())
        } else {
            (quote_ident(Quote::Double, &to), clone_q.clone())
        };
        indexes.push(rewrite_create_index(&sql, sqlite, &index, &on, &t.name, &clone_bare)?);
        renames.push(Rename { from: name, to, shortened: false });
    }
    let mut notes = moved;
    if !left_out.is_empty() {
        let names = |u: bool| left_out.iter().filter(|(_, x)| *x == u).map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
        let (plain, unique) = (names(false), names(true));
        let mut what = Vec::new();
        if !plain.is_empty() {
            what.push(format!("los índices {plain}"));
        }
        if !unique.is_empty() {
            what.push(format!("los índices únicos {unique} (sus valores dejan de ser obligatoriamente únicos en el clon)"));
        }
        notes.push(format!(
            "sin índices: no se crean {}; la clave primaria y las restricciones UNIQUE, que son parte de la tabla, sí",
            what.join(" ni ")
        ));
    }
    Ok(Some(Native { before, create: table.sql, indexes, generated: table.generated, renames, notes }))
}

/// Puts `n` into the plan and the DDL: its statements instead of the ones
/// generated from the catalog, its renames, its computed columns marked so
/// they're never loaded.
pub(super) fn apply(n: Native, ddl: &mut Ddl, plan: &mut ClonePlan, notes: &mut Vec<String>) {
    let mut create: Vec<String> = n.before.iter().map(|(c, _)| c.clone()).collect();
    create.push(n.create);
    ddl.create = create.join(";\n");
    ddl.indexes = (!n.indexes.is_empty()).then(|| n.indexes.join(";\n"));
    // Inside the CREATE (neither engine has ALTER TABLE … ADD FOREIGN KEY).
    ddl.foreign_keys = None;
    for (_, undo) in n.before.iter().rev() {
        ddl.drop = format!("{};\n{undo}", ddl.drop.trim_end().trim_end_matches(';'));
    }
    let mut hidden = Vec::new();
    for g in &n.generated {
        match plan.table.columns.iter_mut().find(|c| same_name(&c.name, g)) {
            // DuckDB reports a computed column as one with a DEFAULT.
            Some(c) => {
                let expr = c.default_value.take().unwrap_or_default();
                c.data_type = format!("{} GENERATED ALWAYS AS ({expr})", c.data_type);
            }
            // SQLite doesn't list them at all.
            None => hidden.push(g.clone()),
        }
    }
    if !hidden.is_empty() {
        notes.push(format!("columnas calculadas por el motor (no se copian, se recalculan): {}", hidden.join(", ")));
    }
    // Unique constraints are part of the CREATE here: only explicit indexes
    // are left out without indexes.
    notes.retain(|x| !x.starts_with("sin índices: tampoco se crean"));
    notes.extend(n.notes);
    plan.renames = n.renames;
}

// -- counters -----------------------------------------------------------------------------------------

/// SQLite's AUTOINCREMENT: `sqlite_sequence` keeps the largest id ever
/// handed out (past the largest row, after deletes). The clone's row gets
/// the original's value. No row: no AUTOINCREMENT, and new rowids follow
/// the largest one, same as in the original.
pub(super) async fn copy_sqlite_sequence(src: &mut dyn Session, tgt: &mut dyn Session, original: &TableSchema, clone: &TableSchema) -> Result<()> {
    let p = sqlite_prefix(original.schema.as_deref());
    let Ok(Some(seq)) = scalar(src, &format!("SELECT seq FROM {p}sqlite_sequence WHERE name = '{}'", lit(&original.name))).await else {
        return Ok(());
    };
    let c = lit(&clone.name);
    let step = async {
        exec(tgt, &format!("DELETE FROM {p}sqlite_sequence WHERE name = '{c}'")).await?;
        exec(tgt, &format!("INSERT INTO {p}sqlite_sequence (name, seq) VALUES ('{c}', {seq})")).await
    };
    step.await.map_err(|e| Error::Query(format!("identidad: {e}")))
}

/// A Firebird identity column: its generator, `ALWAYS` or `BY DEFAULT`,
/// `START WITH` and `INCREMENT BY`.
#[derive(Debug, Clone, PartialEq)]
struct FbIdentity {
    generator: String,
    always: bool,
    initial: i64,
    increment: i64,
}

impl FbIdentity {
    /// The clause that creates it (`INCREMENT BY` only when it isn't 1:
    /// Firebird 3 doesn't take it, and has no other).
    fn clause(&self) -> String {
        let kind = if self.always { "ALWAYS" } else { "BY DEFAULT" };
        let step = if self.increment == 1 { String::new() } else { format!(" INCREMENT BY {}", self.increment) };
        format!("GENERATED {kind} AS IDENTITY (START WITH {}{step})", self.initial)
    }

    fn describe(&self) -> String {
        format!(
            "{}, START WITH {}, INCREMENT BY {}",
            if self.always { "ALWAYS" } else { "BY DEFAULT" },
            self.initial,
            self.increment
        )
    }
}

async fn fb_identity(s: &mut dyn Session, table: &str, col: &str) -> Result<Option<FbIdentity>> {
    let rows = strings(
        s,
        &format!(
            "SELECT TRIM(g.RDB$GENERATOR_NAME), rf.RDB$IDENTITY_TYPE, g.RDB$INITIAL_VALUE, g.RDB$GENERATOR_INCREMENT \
             FROM RDB$RELATION_FIELDS rf JOIN RDB$GENERATORS g ON g.RDB$GENERATOR_NAME = rf.RDB$GENERATOR_NAME \
             WHERE rf.RDB$RELATION_NAME = '{}' AND rf.RDB$FIELD_NAME = '{}'",
            lit(table),
            lit(col)
        ),
    )
    .await
    .map_err(|e| Error::Query(format!("identidad: {e}")))?;
    let Some(r) = rows.into_iter().next() else { return Ok(None) };
    let num = |k: usize| r.get(k).cloned().flatten().and_then(|v| v.trim().parse::<i64>().ok());
    let (Some(generator), Some(initial)) = (r.first().cloned().flatten(), num(2)) else {
        return Err(Error::Unsupported(format!("no se puede clonar: el motor no informa las opciones de la identidad de «{col}»")));
    };
    // Firebird 3: no ALWAYS, and the type may be NULL (BY DEFAULT).
    Ok(Some(FbIdentity { generator, always: num(1) == Some(0), initial, increment: num(3).filter(|i| *i != 0).unwrap_or(1) }))
}

/// Firebird: the CREATE the catalog gives says `GENERATED BY DEFAULT AS
/// IDENTITY`; each identity column gets the original's kind (`ALWAYS`
/// stays `ALWAYS`: the load says `OVERRIDING SYSTEM VALUE`), `START WITH`
/// and `INCREMENT BY`. A column it can't find in the CREATE is refused.
pub(super) async fn firebird_identity_ddl(src: &mut dyn Session, original: &TableSchema, clone: &TableSchema, ddl: &mut Ddl) -> Result<()> {
    for c in original.columns.iter().filter(|c| c.auto_increment) {
        let Some(id) = fb_identity(src, &original.name, &c.name).await? else {
            return Err(Error::Unsupported(format!("no se puede clonar: no se encontró el generador de la identidad de «{}»", c.name)));
        };
        // A counter the clone couldn't continue (the next value is past
        // BIGINT) is refused now, before anything is created.
        let value = format!("SELECT GEN_ID({}, 0) FROM RDB$DATABASE", quote_ident(Quote::Double, &id.generator));
        if let Some(current) = fb_number(src, &value).await? {
            if i64::try_from(current + i128::from(id.increment)).is_err() {
                return Err(Error::Unsupported(format!(
                    "no se puede clonar: el contador de la identidad de «{}» del original está en el límite de BIGINT; el clon no podría seguirlo",
                    c.name
                )));
            }
        }
        let col = clone.columns.iter().find(|x| x.name == c.name).unwrap_or(c);
        let needle = format!("{} {} GENERATED BY DEFAULT AS IDENTITY", quote_ident(Quote::Double, &col.name), col.data_type);
        if !ddl.create.contains(&needle) {
            return Err(Error::Unsupported(format!("no se puede clonar: no se pudo dar a «{}» la identidad del original", c.name)));
        }
        let with = format!("{} {} {}", quote_ident(Quote::Double, &col.name), col.data_type, id.clause());
        ddl.create = ddl.create.replacen(&needle, &with, 1);
    }
    Ok(())
}

/// Firebird: each identity generator of the clone where the original's is
/// (it may be past the largest id, after deletes, or have handed out
/// nothing with rows deleted), stepping like it. `RESTART WITH n` leaves
/// the next value at `n` from Firebird 4 on, and at `n + increment`
/// before: the result is read back and corrected, so it ends equal on
/// either. Then the clone's kind and options are checked against the
/// original's: a difference is refused. A column without a generator:
/// past the largest.
pub(super) async fn firebird_identity(src: &mut dyn Session, tgt: &mut dyn Session, original: &TableSchema, clone: &TableSchema, name: &str) -> Result<Option<String>> {
    let fail = |e: Error| Error::Query(format!("identidad: {e}"));
    for col in clone.columns.iter().filter(|c| c.auto_increment) {
        let col_q = quote_ident(Quote::Double, &col.name);
        // RESTART WITH takes a BIGINT: past it, the clone couldn't follow.
        let restart = |n: i128| -> Result<String> {
            i64::try_from(n).map(|n| format!("ALTER TABLE {name} ALTER COLUMN {col_q} RESTART WITH {n}")).map_err(|_| {
                Error::Unsupported(format!(
                    "no se puede clonar: el contador de la identidad de «{}» del original está en el límite de BIGINT; el clon no podría seguirlo",
                    col.name
                ))
            })
        };
        let og = fb_identity(src, &original.name, &col.name).await?;
        let cg = fb_identity(tgt, &clone.name, &col.name).await?;
        let (og, cg) = match (og, cg) {
            (Some(o), Some(c)) => (o, c),
            (None, None) => {
                if let Some(max) = fb_number(tgt, &format!("SELECT MAX({col_q}) FROM {name}")).await? {
                    exec(tgt, &restart(max + 1)?).await.map_err(fail)?;
                }
                continue;
            }
            _ => return Err(Error::State(format!("la identidad de «{}» no quedó como la del original; no se clona", col.name))),
        };
        let value = |g: &str| format!("SELECT GEN_ID({}, 0) FROM RDB$DATABASE", quote_ident(Quote::Double, g));
        let current =
            fb_number(src, &value(&og.generator)).await?.ok_or_else(|| Error::Query("identidad: no se pudo leer el contador del original".into()))?;
        let mut at = current + i128::from(og.increment);
        let mut done = false;
        for _ in 0..2 {
            exec(tgt, &restart(at)?).await.map_err(fail)?;
            match fb_number(tgt, &value(&cg.generator)).await? {
                Some(n) if n == current => {
                    done = true;
                    break;
                }
                Some(n) => at += current - n,
                None => break,
            }
        }
        if !done {
            return Err(Error::Query(format!("identidad: el contador del clon no quedó igual al del original ({current})")));
        }
        let after = fb_identity(tgt, &clone.name, &col.name).await?;
        let same = after.as_ref().is_some_and(|a| a.always == og.always && a.initial == og.initial && a.increment == og.increment);
        if !same {
            return Err(Error::State(format!(
                "la identidad de «{}» no quedó como la del original ({} en el original, {} en el clon); no se clona",
                col.name,
                og.describe(),
                after.map(|a| a.describe()).unwrap_or_else(|| "sin generador".into())
            )));
        }
    }
    Ok(None)
}

/// A Firebird counter or id, exact: BIGINTs past 2^53 come as text and
/// must never go through `f64`; `i128` so stepping past BIGINT is seen,
/// not overflowed.
async fn fb_number(s: &mut dyn Session, sql: &str) -> Result<Option<i128>> {
    let v = strings(s, sql).await.map_err(|e| Error::Query(format!("identidad: {e}")))?.into_iter().next().and_then(|r| r.into_iter().next().flatten());
    match v {
        None => Ok(None),
        Some(v) => v.trim().parse::<i128>().map(Some).map_err(|_| Error::Query(format!("identidad: el motor devolvió un contador que no se entiende («{v}»)"))),
    }
}

/// Firebird's limit for the names the clone makes up: 63 from Firebird 4 on
/// (characters; counted here in bytes, which is never longer), 31 bytes
/// before or when the version can't be read.
pub(super) async fn firebird_generated_limit(s: &mut dyn Session) -> usize {
    let v = strings(s, "SELECT RDB$GET_CONTEXT('SYSTEM', 'ENGINE_VERSION') FROM RDB$DATABASE")
        .await
        .ok()
        .and_then(|r| r.into_iter().next())
        .and_then(|r| r.into_iter().next().flatten());
    match v.as_deref().and_then(|v| v.trim().split('.').next()?.parse::<u32>().ok()) {
        Some(m) if m >= 4 => 63,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{async_trait, ColumnDef, ConnectionConfig, DriverInfo, Family, Language};

    struct D(DriverInfo);

    #[async_trait]
    impl Driver for D {
        fn info(&self) -> &DriverInfo {
            &self.0
        }
        async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
            Err(Error::Unsupported("test".into()))
        }
    }

    fn d(id: &'static str) -> D {
        D(DriverInfo {
            id,
            name: id,
            family: Family::Relational,
            language: Language::Sql,
            // Both report "standard", not their own name.
            dialect: "standard",
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: true,
            object_kinds: vec![],
        })
    }

    fn table(name: &str) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: name.into(),
            columns: vec![ColumnDef { name: "ID".into(), data_type: "INTEGER".into(), ..Default::default() }],
            indexes: vec![dbine_driver::IndexDef { name: "UQ_A".into(), columns: vec!["ID".into()], unique: true, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn firebird_names_count_characters() {
        let fb = d("firebird");
        // 63 characters, 64 bytes: Firebird 4+ takes it.
        let n63 = format!("{}í", "c".repeat(62));
        assert_eq!((n63.chars().count(), n63.len()), (63, 64));
        assert!(super::super::plan_clone(&fb, &table("T"), &n63).is_ok());
        let e = super::super::plan_clone(&fb, &table("T"), &format!("{n63}x")).unwrap_err().to_string();
        assert!(e.contains("63 caracteres"), "{e}");
    }

    #[test]
    fn duckdb_takes_long_names() {
        assert_eq!(super::super::identifier_limit(&d("duckdb")), 0);
        assert!(super::super::plan_clone(&d("duckdb"), &table("t"), &"x".repeat(300)).is_ok());
    }

    #[test]
    fn firebird_computed_columns_are_not_loaded() {
        let c = ColumnDef { name: "DOBLE".into(), data_type: "COMPUTED BY (A * 2)".into(), ..Default::default() };
        assert!(super::super::generated(&c, "standard"));
    }

    #[test]
    fn sqlite_create_keeps_everything_but_the_name() {
        let sql = "CREATE TABLE \"clonev clí-entes x\" (\n  id INTEGER PRIMARY KEY AUTOINCREMENT,\n  email TEXT NOT NULL COLLATE NOCASE, -- comment, with REFERENCES x\n  \"nom bre\" VARCHAR(80) DEFAULT 'sin (nombre), ok',\n  monto NUMERIC(12,2) CHECK (monto >= 0),\n  padre INTEGER REFERENCES clonev_padres(id) ON DELETE CASCADE,\n  jefe INTEGER REFERENCES \"CLONEV CLÍ-ENTES X\"(id),\n  [doble] AS (monto * 2) STORED,\n  t GENERATED ALWAYS AS (CAST(monto AS TEXT)),\n  CONSTRAINT uq_email UNIQUE (email)\n) STRICT";
        let r = rewrite_create_table(sql, true, Some("main"), "clonev clí-entes x", "\"main\".\"c2\"", "\"c2\"", &[]).unwrap();
        assert!(r.sql.starts_with("CREATE TABLE \"main\".\"c2\" (\n  id INTEGER PRIMARY KEY AUTOINCREMENT,"), "{}", r.sql);
        assert!(r.sql.contains("email TEXT NOT NULL COLLATE NOCASE, -- comment, with REFERENCES x"));
        assert!(r.sql.contains("REFERENCES clonev_padres(id) ON DELETE CASCADE"));
        // The self reference (any case) points to the clone.
        assert!(r.sql.contains("jefe INTEGER REFERENCES \"c2\"(id),"), "{}", r.sql);
        assert!(r.sql.contains("CONSTRAINT uq_email UNIQUE (email)\n) STRICT"));
        assert_eq!(r.generated, ["doble", "t"]);
        assert!(r.sequences.is_empty());

        let ix = "CREATE UNIQUE INDEX clonev_ux ON \"clonev clí-entes x\" (padre, \"nom bre\" COLLATE NOCASE) WHERE padre IS NOT NULL";
        assert_eq!(
            rewrite_create_index(ix, true, "\"main\".\"c2_ux\"", "\"c2\"", "clonev clí-entes x", "\"c2\"").unwrap(),
            "CREATE UNIQUE INDEX \"main\".\"c2_ux\" ON \"c2\" (padre, \"nom bre\" COLLATE NOCASE) WHERE padre IS NOT NULL"
        );
        // Not a CREATE it understands: refused, never guessed.
        assert!(rewrite_create_table("CREATE VIEW v AS SELECT 1", true, None, "v", "x", "x", &[]).is_err());
        assert!(rewrite_create_table("CREATE TABLE t (a 'unterminated)", true, None, "t", "x", "x", &[]).is_err());
    }

    #[test]
    fn columns_qualified_with_the_table_name_are_requalified() {
        // SQLite / libSQL: CHECK, computed column, self reference.
        let sql = "CREATE TABLE tq (k TEXT PRIMARY KEY, v INT, w INT AS (TQ.v * 2), p INT REFERENCES tq(k), CHECK (tq.v >= 0), CHECK (\"tq\".v < main.tq.w)) WITHOUT ROWID";
        let r = rewrite_create_table(sql, true, Some("main"), "tq", "\"main\".\"tq2\"", "\"tq2\"", &[]).unwrap();
        assert_eq!(
            r.sql,
            "CREATE TABLE \"main\".\"tq2\" (k TEXT PRIMARY KEY, v INT, w INT AS (\"tq2\".v * 2), p INT REFERENCES \"tq2\"(k), CHECK (\"tq2\".v >= 0), CHECK (\"tq2\".v < main.\"tq2\".w)) WITHOUT ROWID"
        );
        // A schema named like the table, after REFERENCES, stays a schema;
        // a column named like the table, or a string, stays too.
        let sql = "CREATE TABLE tq (tq INT, v INT REFERENCES tq.other(id), CHECK (tq > 0 AND v <> 'tq.v'))";
        let r = rewrite_create_table(sql, true, None, "tq", "\"tq2\"", "\"tq2\"", &[]).unwrap();
        assert_eq!(r.sql, "CREATE TABLE \"tq2\" (tq INT, v INT REFERENCES tq.other(id), CHECK (tq > 0 AND v <> 'tq.v'))");
        // DuckDB keeps the qualifier in its stored CREATE.
        let sql = "CREATE TABLE tq(k VARCHAR PRIMARY KEY, v INTEGER, CHECK((tq.v >= 0)));";
        let r = rewrite_create_table(sql, false, Some("main"), "tq", "main.\"tq2\"", "\"tq2\"", &[]).unwrap();
        assert_eq!(r.sql, "CREATE TABLE main.\"tq2\"(k VARCHAR PRIMARY KEY, v INTEGER, CHECK((\"tq2\".v >= 0)));");
        // Partial and expression indexes.
        assert_eq!(
            rewrite_create_index("CREATE INDEX ix_tq ON tq (lower(tq.k)) WHERE tq.v > 0", true, "\"ix_tq2\"", "\"tq2\"", "tq", "\"tq2\"").unwrap(),
            "CREATE INDEX \"ix_tq2\" ON \"tq2\" (lower(\"tq2\".k)) WHERE \"tq2\".v > 0"
        );
        // Firebird: the catalog's expressions.
        assert_eq!(requalify("(ADV4.V >= 0 AND adv4.\"W\" < 9)", "ADV4", "\"ADV4_C\""), "(\"ADV4_C\".V >= 0 AND \"ADV4_C\".\"W\" < 9)");
        assert_eq!(requalify("ADV4 > 0", "ADV4", "\"ADV4_C\""), "ADV4 > 0");
        // A subquery that reads the table itself reads the clone.
        assert_eq!(
            requalify("NOT EXISTS (SELECT 1 FROM ADV5 X WHERE X.A = ADV5.A AND X.ID <> ADV5.ID)", "ADV5", "\"ADV5_C\""),
            "NOT EXISTS (SELECT 1 FROM \"ADV5_C\" X WHERE X.A = \"ADV5_C\".A AND X.ID <> \"ADV5_C\".ID)"
        );
        assert_eq!(
            requalify("((SELECT COUNT(*) FROM ADV6 Z WHERE Z.P = ADV6.ID))", "ADV6", "\"ADV6_C\""),
            "((SELECT COUNT(*) FROM \"ADV6_C\" Z WHERE Z.P = \"ADV6_C\".ID))"
        );
        assert_eq!(
            requalify("(SELECT MAX(B.V) FROM OTHER A, \"ADV6\" AS B JOIN ADV6 ON ADV6.ID = A.ID WHERE ADV6.V > 0)", "ADV6", "\"ADV6_C\""),
            "(SELECT MAX(B.V) FROM OTHER A, \"ADV6_C\" AS B JOIN \"ADV6_C\" ON \"ADV6_C\".ID = A.ID WHERE \"ADV6_C\".V > 0)"
        );
        assert!(reads_table("(SELECT 1 FROM adv6)", "ADV6"));
        // EXTRACT / SUBSTRING's FROM names no table; a table named like
        // another, or a column named like the table, stays.
        assert_eq!(requalify("EXTRACT(YEAR FROM ADV6.D) > 0 AND SUBSTRING(ADV6 FROM 1) <> ''", "ADV6", "\"ADV6_C\""), "EXTRACT(YEAR FROM \"ADV6_C\".D) > 0 AND SUBSTRING(ADV6 FROM 1) <> ''");
        assert!(!reads_table("EXTRACT(YEAR FROM ADV6) > 0", "ADV6"));
        assert_eq!(requalify("(SELECT 1 FROM ADV66 WHERE ADV66.ADV6 = 1)", "ADV6", "\"ADV6_C\""), "(SELECT 1 FROM ADV66 WHERE ADV66.ADV6 = 1)");
    }

    #[test]
    fn firebird_plan_requalifies_the_catalog_expressions() {
        let mut t = table("ADV4");
        t.checks = vec![dbine_driver::CheckDef { name: Some("CK_ADV4_V".into()), expression: "(ADV4.ID >= 0)".into() }];
        t.columns.push(ColumnDef { name: "D".into(), data_type: "COMPUTED BY (ADV4.ID * 2)".into(), ..Default::default() });
        t.indexes.push(dbine_driver::IndexDef {
            name: "IX_ADV4_E".into(),
            columns: vec!["(ADV4.ID + 1)".into()],
            kind: Some("COMPUTED".into()),
            filter: Some("ADV4.ID > 0".into()),
            ..Default::default()
        });
        let p = super::super::plan_clone(&d("firebird"), &t, "ADV4_C").unwrap();
        assert_eq!(p.table.checks[0].expression, "(\"ADV4_C\".ID >= 0)");
        assert_eq!(p.table.checks[0].name.as_deref(), Some("CK_ADV4_C_V"));
        assert_eq!(p.table.columns[1].data_type, "COMPUTED BY (\"ADV4_C\".ID * 2)");
        assert_eq!(p.table.indexes[1].columns, ["(\"ADV4_C\".ID + 1)"]);
        assert_eq!(p.table.indexes[1].filter.as_deref(), Some("\"ADV4_C\".ID > 0"));
        assert!(!p.notes.iter().any(|n| n.contains("misma tabla")), "{:?}", p.notes);
        // A CHECK or a computed column that reads the table reads the clone.
        let mut t5 = table("ADV5");
        t5.checks = vec![dbine_driver::CheckDef { name: Some("CK_ADV5_U".into()), expression: "(NOT EXISTS (SELECT 1 FROM ADV5 X WHERE X.ID <> ADV5.ID))".into() }];
        t5.columns.push(ColumnDef { name: "T".into(), data_type: "COMPUTED BY ((SELECT COUNT(*) FROM ADV5 Z))".into(), ..Default::default() });
        let p = super::super::plan_clone(&d("firebird"), &t5, "ADV5_C").unwrap();
        assert_eq!(p.table.checks[0].expression, "(NOT EXISTS (SELECT 1 FROM \"ADV5_C\" X WHERE X.ID <> \"ADV5_C\".ID))");
        assert_eq!(p.table.columns[1].data_type, "COMPUTED BY ((SELECT COUNT(*) FROM \"ADV5_C\" Z))");
        assert!(p.notes.iter().any(|n| n.contains("CK_ADV5_C_U") && n.contains("columna calculada T") && n.contains("leen la misma tabla")), "{:?}", p.notes);
        // A name another table's object has gets a new one.
        let p = super::super::plan_clone_with(&d("firebird"), &t, "ADV4_C", &["CK_ADV4_C_V".into()]).unwrap();
        assert!(p.table.checks[0].name.as_deref().is_some_and(|n| n.starts_with("CK_ADV4_C_V_") && n.len() <= 31), "{:?}", p.table.checks[0].name);
    }

    #[test]
    fn firebird_taken_names_read_indexes_and_constraints() {
        let sql = firebird_taken_sql(&["CK_ADV3_C_V".into(), "IX_O'K".into()]);
        assert!(sql.contains("FROM RDB$INDICES WHERE RDB$INDEX_NAME IN ('CK_ADV3_C_V', 'IX_O''K')"), "{sql}");
        assert!(sql.contains("FROM RDB$RELATION_CONSTRAINTS WHERE RDB$CONSTRAINT_NAME IN ('CK_ADV3_C_V', 'IX_O''K')"), "{sql}");
    }

    #[test]
    fn firebird_identity_clause_keeps_kind_and_options() {
        let id = |always, initial, increment| FbIdentity { generator: "RDB$1".into(), always, initial, increment };
        assert_eq!(id(true, 100, 3).clause(), "GENERATED ALWAYS AS IDENTITY (START WITH 100 INCREMENT BY 3)");
        // Firebird 3 takes no INCREMENT BY: left out when it's 1.
        assert_eq!(id(false, 0, 1).clause(), "GENERATED BY DEFAULT AS IDENTITY (START WITH 0)");
        assert_eq!(id(false, -5, -2).clause(), "GENERATED BY DEFAULT AS IDENTITY (START WITH -5 INCREMENT BY -2)");
    }

    #[test]
    fn sqlite_prefix_is_refused_up_front() {
        for id in ["sqlite", "libsql"] {
            let d = dbine_drivers::find(id).unwrap();
            for n in ["sqlite_x", "SQLITE_sequence", "sqlite_"] {
                let e = name_problem(d.info(), n).unwrap_or_else(|| panic!("{id} {n}"));
                assert!(e.contains("empiezan con «sqlite_»"), "{e}");
            }
            assert!(name_problem(d.info(), "sqlite").is_none());
            assert!(name_problem(d.info(), "mi_sqlite_x").is_none());
            assert!(name_problem(d.info(), "ñsqlite_").is_none());
            assert!(name_problem(d.info(), "abcdefñ").is_none());
        }
        let duck = dbine_drivers::find("duckdb").unwrap();
        assert!(name_problem(duck.info(), "sqlite_x").is_none());
    }

    #[test]
    fn duckdb_create_gets_its_own_sequences() {
        let sql = "CREATE TABLE \"ventas ñ\".\"clí-entes x\"(id INTEGER DEFAULT(nextval('\"ventas ñ\".seq_cli')) PRIMARY KEY, k INTEGER GENERATED ALWAYS AS(42), d INTEGER GENERATED ALWAYS AS(CAST((monto * 2) AS INTEGER)), monto INTEGER, e VARCHAR COLLATE NOCASE, tags VARCHAR[] DEFAULT(['a', 'b']), jefe INTEGER REFERENCES \"ventas ñ\".\"clí-entes x\"(id));";
        let seqs = [("\"ventas ñ\".seq_cli".to_string(), "\"ventas ñ\".\"c2_seq_cli\"".to_string())];
        let r = rewrite_create_table(sql, false, Some("ventas ñ"), "clí-entes x", "\"ventas ñ\".\"c2\"", "\"c2\"", &seqs).unwrap();
        assert_eq!(r.sequences, ["\"ventas ñ\".seq_cli"]);
        assert_eq!(r.generated, ["k", "d"]);
        assert!(r.sql.starts_with("CREATE TABLE \"ventas ñ\".\"c2\"(id INTEGER DEFAULT(nextval('\"ventas ñ\".\"c2_seq_cli\"')) PRIMARY KEY"), "{}", r.sql);
        assert!(r.sql.contains("e VARCHAR COLLATE NOCASE, tags VARCHAR[] DEFAULT(['a', 'b'])"));
        assert!(r.sql.contains("REFERENCES \"ventas ñ\".\"c2\"(id));"), "{}", r.sql);
        // A reference to a same-named table in another schema stays.
        let other = sql.replace("REFERENCES \"ventas ñ\"", "REFERENCES otro");
        let r = rewrite_create_table(&other, false, Some("ventas ñ"), "clí-entes x", "\"ventas ñ\".\"c2\"", "\"c2\"", &seqs).unwrap();
        assert!(r.sql.contains("REFERENCES otro.\"clí-entes x\"(id)"), "{}", r.sql);
        assert_eq!(
            rewrite_create_index("CREATE INDEX \"ix_clí-entes x_nombre\" ON \"ventas ñ\".\"clí-entes x\"(e);", false, "\"ix_c2_nombre\"", "\"ventas ñ\".\"c2\"", "clí-entes x", "\"c2\"").unwrap(),
            "CREATE INDEX \"ix_c2_nombre\" ON \"ventas ñ\".\"c2\"(e);"
        );
    }
}
