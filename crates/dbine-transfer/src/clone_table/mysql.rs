//! MySQL, MariaDB and TiDB: the clone is written from the original's own
//! `SHOW CREATE TABLE`, not from `database_schema`, which flattens what
//! the clone must keep: per-column CHARACTER SET / COLLATE, INVISIBLE
//! columns, partitions, ROW_FORMAT and other table options, TiDB's
//! AUTO_RANDOM and clustered keys, CHECKs with any name.
//!
//! - the CREATE takes the columns, the primary key, the CHECKs and the
//!   table options as the server printed them, under the new name; the
//!   indexes (after the rows) and the foreign keys (last) go as
//!   `ALTER TABLE … ADD` of the same lines; every named constraint and
//!   index renamed like the others (see [`super::rename_constraint`]);
//! - refused: system-versioned tables (MariaDB; their history can't be
//!   copied) and, on TiDB, CHECKs while `tidb_enable_check_constraint`
//!   is off (the server would drop them silently);
//! - after the CREATE and again at the end, the clone's `SHOW CREATE
//!   TABLE` must say the same as the original's (names mapped back,
//!   counters aside): anything lost drops the clone.

use super::{fnv, strings, Rename};
use dbine_driver::{Driver, Error, Result, Session};
use std::collections::HashMap;

/// Engines whose `SHOW CREATE TABLE` is checked live to rebuild the table.
const IDS: &[&str] = &["mysql", "mariadb", "tidb", "aurora-mysql", "cloudsql-mysql"];

pub(super) fn applies(driver: &dyn Driver) -> bool {
    IDS.contains(&driver.info().id)
}

/// What an element of the table's body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Column,
    Primary,
    Index,
    ForeignKey,
    Check,
    Other,
}

/// One element of the body (a column, a key, a constraint), as printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Part {
    pub kind: Kind,
    pub text: String,
    /// The element's name: its byte range in `text` (quotes included) and
    /// its value.
    name: Option<(usize, usize, String)>,
}

/// A `SHOW CREATE TABLE`, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShowCreate {
    pub parts: Vec<Part>,
    /// From the `)` that closes the body on: table options, partitions.
    pub tail: String,
}

fn quote(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// The backtick-quoted identifier starting at byte `at`: its value and the
/// byte after its closing quote.
fn ident_at(s: &str, at: usize) -> Option<(String, usize)> {
    let rest = s.get(at..)?;
    if !rest.starts_with('`') {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        if c == '`' {
            if chars.peek().map(|(_, n)| *n) == Some('`') {
                chars.next();
                out.push('`');
            } else {
                return Some((out, at + i + 1));
            }
        } else {
            out.push(c);
        }
    }
    None
}

/// Quote tracking over the server's text: backticks (`` ` `` doubled),
/// `'…'` and `"…"` (quote doubled or backslash-escaped).
#[derive(Debug, Default, Clone, Copy)]
struct Quotes {
    open: Option<char>,
    escaped: bool,
}

impl Quotes {
    /// Takes `c` in; whether it was outside any quote (a quote that opens
    /// counts as inside).
    fn step(&mut self, c: char) -> bool {
        match self.open {
            Some(_) if self.escaped => self.escaped = false,
            Some(q) if q != '`' && c == '\\' => self.escaped = true,
            Some(q) if c == q => self.open = None,
            Some(_) => {}
            None if matches!(c, '`' | '\'' | '"') => self.open = Some(c),
            None => return true,
        }
        false
    }
}

/// Where `pat` starts in `s` after `from`, outside backticks and quotes.
fn find_outside(s: &str, from: usize, pat: &str) -> Option<usize> {
    let mut q = Quotes::default();
    for (i, c) in s[from..].char_indices() {
        let at = from + i;
        if q.open.is_none() && s[at..].starts_with(pat) {
            return Some(at);
        }
        q.step(c);
    }
    None
}

const INDEX_PREFIXES: &[&str] = &[
    "UNIQUE KEY ",
    "UNIQUE INDEX ",
    "FULLTEXT KEY ",
    "FULLTEXT INDEX ",
    "SPATIAL KEY ",
    "SPATIAL INDEX ",
    "VECTOR KEY ",
    "VECTOR INDEX ",
    "KEY ",
    "INDEX ",
];

fn classify(text: String) -> Part {
    let named = |kind: Kind, at: usize, text: String| {
        let name = ident_at(&text, at).map(|(n, end)| (at, end, n));
        Part { kind, text, name }
    };
    if text.starts_with('`') {
        return Part { kind: Kind::Column, text, name: None };
    }
    if text.starts_with("PRIMARY KEY") {
        return Part { kind: Kind::Primary, text, name: None };
    }
    if let Some(rest) = text.strip_prefix("CONSTRAINT ") {
        let at = text.len() - rest.len();
        let after = ident_at(&text, at).map(|(_, end)| text[end..].trim_start().to_string()).unwrap_or_default();
        let kind = if after.starts_with("FOREIGN KEY") {
            Kind::ForeignKey
        } else if after.starts_with("CHECK") {
            Kind::Check
        } else {
            Kind::Other
        };
        return named(kind, at, text);
    }
    if let Some(p) = INDEX_PREFIXES.iter().find(|p| text.starts_with(**p)) {
        return named(Kind::Index, p.len(), text);
    }
    Part { kind: Kind::Other, text, name: None }
}

/// A `SHOW CREATE TABLE` taken apart; `None` when it doesn't look like one
/// (the caller refuses rather than guess).
pub(super) fn parse(sql: &str) -> Option<ShowCreate> {
    let mut lines = sql.split('\n');
    let head = lines.next()?.trim_end();
    if !head.starts_with("CREATE TABLE ") || !head.ends_with('(') {
        return None;
    }
    let rest: Vec<&str> = lines.collect();
    let mut elems: Vec<String> = Vec::new();
    let mut tail = None;
    // TiDB prints newlines inside string literals as they are (MySQL and
    // MariaDB as `\n`): a line that starts inside a quote continues the
    // element, exactly as printed.
    let mut q = Quotes::default();
    for (i, l) in rest.iter().enumerate() {
        let inside = q.open.is_some();
        if !inside && l.starts_with(')') {
            tail = Some(rest[i..].join("\n"));
            break;
        }
        match elems.last_mut() {
            // A line that isn't indented, after an element that doesn't end
            // with a comma, continues it (TiDB prints its CONSTRAINT lines
            // without indentation).
            Some(last) if inside || (!l.starts_with("  ") && !last.ends_with(',')) => {
                last.push('\n');
                last.push_str(l);
            }
            _ => elems.push(l.trim_start().to_string()),
        }
        for c in l.chars().chain(['\n']) {
            q.step(c);
        }
    }
    let tail = tail?;
    let n = elems.len();
    let mut parts = Vec::with_capacity(n);
    for (i, mut e) in elems.into_iter().enumerate() {
        if i + 1 < n {
            e = e.strip_suffix(',')?.to_string();
        }
        parts.push(classify(e));
    }
    (!parts.is_empty()).then_some(ShowCreate { parts, tail })
}

/// The table a foreign key references: byte range of the table's quoted
/// name in `text`, the schema (when qualified) and the table.
fn referenced(text: &str, from: usize) -> Option<(usize, usize, Option<String>, String)> {
    let at = find_outside(text, from, " REFERENCES ")? + " REFERENCES ".len();
    let (a, end) = ident_at(text, at)?;
    if text[end..].starts_with('.') {
        let (b, end2) = ident_at(text, end + 1)?;
        Some((end + 1, end2, Some(a), b))
    } else {
        Some((at, end, None, a))
    }
}

/// `part` with its name given by `name_of` and, for a foreign key to the
/// table `from` itself (in `database` or unqualified), pointing to `to`.
fn rewrite(part: &Part, name_of: &dyn Fn(&str) -> Option<String>, from: &str, to: &str, database: Option<&str>) -> String {
    let mut text = part.text.clone();
    let mut name_end = 0;
    if let Some((s, e, n)) = &part.name {
        name_end = *e;
        if let Some(new) = name_of(n) {
            text = format!("{}{}{}", &part.text[..*s], quote(&new), &part.text[*e..]);
            name_end = s + quote(&new).len();
        }
    }
    if part.kind == Kind::ForeignKey {
        if let Some((s, e, schema, table)) = referenced(&text, name_end) {
            let same_db = schema.as_deref().is_none_or(|x| database.is_some_and(|d| d == x));
            if table == from && same_db {
                text = format!("{}{}{}", &text[..s], quote(to), &text[e..]);
            }
        }
    }
    text
}

/// The first column of an index's key (`KEY `n` (`c`, …)`).
fn first_key_column(part: &Part) -> Option<String> {
    let end = part.name.as_ref().map(|(_, e, _)| *e).unwrap_or(0);
    let rest = part.text[end..].trim_start();
    let open = part.text.len() - rest.len();
    rest.starts_with('(').then(|| ident_at(&part.text, open + 1).map(|(n, _)| n)).flatten()
}

/// The original, as the server prints it.
pub(super) struct Original {
    pub create: ShowCreate,
    /// The session's database (TiDB qualifies references with it).
    pub database: Option<String>,
    /// TiDB wraps a CHECK's condition in parentheses once more each time
    /// it's created from its own output: one level is taken off.
    pub tidb: bool,
}

/// Read the original's `SHOW CREATE TABLE` and refuse what can't be cloned
/// faithfully. Nothing is written.
pub(super) async fn inspect(driver: &dyn Driver, s: &mut dyn Session, name: &str) -> Result<Original> {
    let rows = strings(s, &format!("SHOW CREATE TABLE {}", quote(name))).await?;
    let text = rows.into_iter().next().and_then(|r| r.into_iter().nth(1).flatten()).unwrap_or_default();
    let create = parse(&text).ok_or_else(|| {
        Error::Unsupported(format!("no se puede clonar: no se pudo interpretar la definición de «{name}» (SHOW CREATE TABLE)"))
    })?;
    refuse(&create)?;
    if driver.info().id == "tidb" && create.parts.iter().any(|p| p.kind == Kind::Check) {
        let on = strings(s, "SELECT @@global.tidb_enable_check_constraint").await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next().flatten());
        if !matches!(on.as_deref().map(str::trim), Some("1") | Some("ON") | Some("on")) {
            return Err(Error::Unsupported(
                "no se puede clonar: la tabla tiene restricciones CHECK y el servidor las tiene desactivadas (tidb_enable_check_constraint = OFF); el clon las perdería. Activalas con SET GLOBAL tidb_enable_check_constraint = ON y volvé a intentar".into(),
            ));
        }
    }
    let database = strings(s, "SELECT DATABASE()").await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next().flatten());
    Ok(Original { create, database, tidb: driver.info().id == "tidb" })
}

/// What a faithful clone can't be made of.
pub(super) fn refuse(c: &ShowCreate) -> Result<()> {
    let upper = |s: &str| s.to_ascii_uppercase();
    if upper(&c.tail).contains("WITH SYSTEM VERSIONING")
        || c.parts.iter().any(|p| upper(&p.text).contains("WITH SYSTEM VERSIONING") || upper(&p.text).starts_with("PERIOD FOR SYSTEM_TIME"))
    {
        return Err(Error::Unsupported(
            "no se puede clonar: la tabla tiene versionado de sistema (WITH SYSTEM VERSIONING); su historial no se puede copiar y el clon no sería igual".into(),
        ));
    }
    Ok(())
}

/// The clone's DDL, from the original's own definition.
pub(super) struct Ddl {
    pub create: String,
    pub indexes: Option<String>,
    pub foreign_keys: Option<String>,
    pub notes: Vec<String>,
}

/// The clone's DDL: `sql_name` is its quoted name, `auto` the columns
/// with AUTO_INCREMENT (the index they lead stays in the CREATE: the
/// server requires it). Names not in `renames` yet (the catalog didn't
/// report them) are renamed here and added.
#[allow(clippy::too_many_arguments)]
pub(super) fn ddl(o: &Original, renames: &mut Vec<Rename>, old: &str, new: &str, sql_name: &str, auto: &[String], with_indexes: bool, max: usize) -> Ddl {
    let notes = complete(o, renames, old, new, max, &[]);
    let map: HashMap<String, String> = renames.iter().map(|r| (r.from.clone(), r.to.clone())).collect();
    let name_of = |n: &str| map.get(n).cloned();
    let db = o.database.as_deref();
    let mut body = Vec::new();
    let mut indexes = Vec::new();
    let mut fulltext = Vec::new();
    let mut fks = Vec::new();
    for p in &o.create.parts {
        let mut text = rewrite(p, &name_of, old, new, db);
        if o.tidb && p.kind == Kind::Check {
            text = unwrap_check(&text);
        }
        match p.kind {
            Kind::Index if !first_key_column(p).is_some_and(|c| auto.contains(&c)) => {
                if with_indexes {
                    if text.starts_with("FULLTEXT ") {
                        fulltext.push(format!("ADD {text}"));
                    } else {
                        indexes.push(format!("ADD {text}"));
                    }
                }
            }
            Kind::ForeignKey => fks.push(format!("ADD {text}")),
            _ => body.push(text),
        }
    }
    let create = format!("CREATE TABLE {sql_name} (\n  {}\n{}", body.join(",\n  "), o.create.tail);
    let alter = |v: Vec<String>| (!v.is_empty()).then(|| format!("ALTER TABLE {sql_name} {}", v.join(", ")));
    // InnoDB (MySQL) adds one FULLTEXT index per ALTER: with more than one,
    // each goes in its own statement.
    let indexes = if fulltext.len() > 1 {
        let mut stmts: Vec<String> = alter(indexes).into_iter().collect();
        stmts.extend(fulltext.into_iter().map(|f| format!("ALTER TABLE {sql_name} {f}")));
        Some(stmts.join(";\n"))
    } else {
        indexes.extend(fulltext);
        alter(indexes)
    };
    Ddl { create, indexes, foreign_keys: alter(fks), notes }
}

/// Renames for the names in the definition that `renames` doesn't have yet
/// (the catalog didn't report them), never one in `reserved` (another
/// object of the schema has it; compared ignoring case). The notes say
/// which were shortened or moved.
pub(super) fn complete(o: &Original, renames: &mut Vec<Rename>, old: &str, new: &str, max: usize, reserved: &[String]) -> Vec<String> {
    let mut notes = Vec::new();
    for p in &o.create.parts {
        let Some((_, _, n)) = &p.name else { continue };
        if renames.iter().any(|r| &r.from == n) {
            continue;
        }
        // MySQL, MariaDB and TiDB count names in characters (TiDB's CHECK
        // names: also 64 bytes).
        let bytes = if o.tidb { super::byte_cap("tidb") } else { 0 };
        let (mut to, shortened) = super::rename_capped(n, old, new, max, true, bytes);
        let first = to.clone();
        let mut k = 0u32;
        while renames.iter().map(|r| &r.to).chain(reserved).any(|u| u.eq_ignore_ascii_case(&to)) {
            k += 1;
            let suffix = format!("_{:08x}", fnv(&format!("{n}#{k}")));
            let base = super::fit(&to, max.saturating_sub(suffix.len()), true, bytes.saturating_sub(suffix.len())).to_string();
            to = format!("{base}{suffix}");
        }
        if shortened {
            notes.push(format!("nombre acortado para entrar en el límite de {} del motor: {n} → {to}", super::limit_text(max, true, bytes)));
        }
        if reserved.iter().any(|u| u.eq_ignore_ascii_case(&first)) {
            notes.push(format!("nombre que ya usaba otro objeto del esquema, cambiado: {n} → {to}"));
        }
        renames.push(Rename { from: n.clone(), to, shortened });
    }
    notes
}

/// `… CHECK ((cond)) …` as `… CHECK (cond) …` when the inner parentheses
/// hold the whole condition (not `CHECK ((a) OR (b))`).
fn unwrap_check(text: &str) -> String {
    let Some(at) = find_outside(text, 0, "CHECK ((") else { return text.to_string() };
    let open = at + "CHECK (".len();
    let close_of = |from: usize| {
        let mut depth = 0i32;
        let mut q = Quotes::default();
        for (i, c) in text[from..].char_indices() {
            if !q.step(c) {
                continue;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
                if depth == 0 {
                    return Some(from + i);
                }
            }
        }
        None
    };
    match (close_of(open), close_of(open - 1)) {
        (Some(inner), Some(outer)) if outer == inner + 1 => format!("{}{}{}", &text[..open], &text[open + 1..inner], &text[inner + 1..]),
        _ => text.to_string(),
    }
}

/// Table options whose number moves with use (counters): not compared.
fn without_counters(tail: &str) -> String {
    tail.split(' ')
        .filter(|w| {
            let w = w.trim_start_matches("/*T![auto_rand_base]");
            !(w.starts_with("AUTO_INCREMENT=") || w.starts_with("AUTO_RANDOM_BASE="))
        })
        .collect::<Vec<_>>()
        .join(" ")
        .replace("/*T![auto_rand_base] */", "")
        .replace("  ", " ")
}

/// A definition reduced to what must be equal: columns in order, the other
/// elements as a sorted list, the table options without counters.
fn comparable(c: &ShowCreate, name_of: &dyn Fn(&str) -> Option<String>, from: &str, to: &str, db: Option<&str>, keep: &dyn Fn(Kind) -> bool) -> (Vec<String>, Vec<String>, String) {
    let mut cols = Vec::new();
    let mut rest = Vec::new();
    for p in c.parts.iter().filter(|p| keep(p.kind)) {
        let t = rewrite(p, name_of, from, to, db);
        if p.kind == Kind::Column {
            cols.push(t);
        } else {
            rest.push(t);
        }
    }
    rest.sort();
    (cols, rest, without_counters(&c.tail))
}

/// Which stage of the clone is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    /// Right after the CREATE: columns, keys, CHECKs, options.
    Created,
    /// At the end: indexes (when they were asked for) and foreign keys too.
    Done { with_indexes: bool },
}

/// The differences between the original's definition and the clone's
/// (names mapped back), in Spanish; empty when equal.
pub(super) fn differences(original: &ShowCreate, clone: &ShowCreate, renames: &[Rename], old: &str, new: &str, db: Option<&str>, stage: Stage) -> Vec<String> {
    let back: HashMap<&str, &str> = renames.iter().map(|r| (r.to.as_str(), r.from.as_str())).collect();
    let keep = |k: Kind| match stage {
        Stage::Created => !matches!(k, Kind::Index | Kind::ForeignKey),
        Stage::Done { with_indexes } => with_indexes || k != Kind::Index,
    };
    let same = |_: &str| None;
    let a = comparable(original, &same, old, old, db, &keep);
    let b = comparable(clone, &|n: &str| back.get(n).map(|s| s.to_string()), new, old, db, &keep);
    let show = |s: &str| {
        let s = s.replace('\n', " ");
        if s.chars().count() > 160 {
            format!("{}…", s.chars().take(160).collect::<String>())
        } else {
            s
        }
    };
    let mut out = Vec::new();
    if a.0 != b.0 {
        for (x, y) in a.0.iter().zip(&b.0).filter(|(x, y)| x != y).take(3) {
            out.push(format!("original «{}», clon «{}»", show(x), show(y)));
        }
        if a.0.len() != b.0.len() {
            out.push(format!("{} columnas en el original, {} en el clon", a.0.len(), b.0.len()));
        }
    }
    if a.1 != b.1 {
        let mut only_b = b.1.clone();
        let mut only_a = Vec::new();
        for x in &a.1 {
            match only_b.iter().position(|y| y == x) {
                Some(i) => {
                    only_b.remove(i);
                }
                None => only_a.push(x.clone()),
            }
        }
        for x in only_a.iter().take(3) {
            out.push(format!("falta en el clon «{}»", show(x)));
        }
        for y in only_b.iter().take(3) {
            out.push(format!("sobra en el clon «{}»", show(y)));
        }
    }
    if a.2 != b.2 {
        out.push(format!("opciones de la tabla: original «{}», clon «{}»", show(a.2.trim_start_matches(')').trim()), show(b.2.trim_start_matches(')').trim())));
    }
    out
}

/// The clone's `SHOW CREATE TABLE` against the original's: anything
/// different refuses the clone (the caller drops it).
pub(super) async fn verify(s: &mut dyn Session, o: &Original, renames: &[Rename], old: &str, new: &str, stage: Stage) -> Result<()> {
    let rows = strings(s, &format!("SHOW CREATE TABLE {}", quote(new))).await?;
    let text = rows.into_iter().next().and_then(|r| r.into_iter().nth(1).flatten()).unwrap_or_default();
    let clone = parse(&text).ok_or_else(|| Error::State("no se pudo leer la definición del clon para compararla con la del original; no se clona".into()))?;
    let diffs = differences(&o.create, &clone, renames, old, new, o.database.as_deref(), stage);
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(Error::State(format!("el clon no quedó igual al original ({}); no se clona", diffs.join("; "))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MYSQL: &str = "CREATE TABLE `cli``ente ñ` (
  `id` int NOT NULL AUTO_INCREMENT,
  `e-mail` varchar(100) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin DEFAULT NULL,
  `nombre` varchar(50) CHARACTER SET latin1 COLLATE latin1_spanish_ci NOT NULL DEFAULT 'x, y',
  `total_iva` decimal(14,2) GENERATED ALWAYS AS ((`saldo` * 1.21)) STORED,
  `otro` int DEFAULT NULL,
  `oculto` int DEFAULT NULL /*!80023 INVISIBLE */,
  PRIMARY KEY (`id`),
  UNIQUE KEY `ux_mail` (`e-mail`),
  KEY `fk_self` (`otro`),
  KEY `ix_fn` ((lower(`nombre`))),
  CONSTRAINT `fk_self` FOREIGN KEY (`otro`) REFERENCES `cli``ente ñ` (`id`),
  CONSTRAINT `chk_cli``ente ñ_saldo` CHECK ((`saldo` >= 0))
) ENGINE=InnoDB AUTO_INCREMENT=7 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci ROW_FORMAT=COMPRESSED
/*!50100 PARTITION BY KEY (`id`)
PARTITIONS 3 */";

    fn original() -> Original {
        Original { create: parse(MYSQL).unwrap(), database: Some("db".into()), tidb: false }
    }

    #[test]
    fn completed_names_avoid_the_schemas() {
        let o = original();
        let mut renames = Vec::new();
        let notes = complete(&o, &mut renames, "cli`ente ñ", "copia", 64, &["COPIA_FK_SELF".into(), "otro".into()]);
        let fk = renames.iter().find(|r| r.from == "fk_self").unwrap().clone();
        assert!(fk.to.starts_with("copia_fk_self_"), "{renames:?}");
        assert!(notes.iter().any(|n| n.contains("fk_self →")), "{notes:?}");
        // Already complete: nothing more, and `ddl` keeps them.
        let before = renames.clone();
        let d = ddl(&o, &mut renames, "cli`ente ñ", "copia", "`copia`", &["id".into()], true, 64);
        assert_eq!(renames.len(), before.len());
        assert!(d.foreign_keys.unwrap().contains(&format!("`{}`", fk.to)));
    }

    #[test]
    fn takes_a_definition_apart() {
        let c = parse(MYSQL).unwrap();
        let kinds: Vec<Kind> = c.parts.iter().map(|p| p.kind).collect();
        use Kind::*;
        assert_eq!(kinds, vec![Column, Column, Column, Column, Column, Column, Primary, Index, Index, Index, ForeignKey, Check]);
        assert_eq!(c.parts[11].name.as_ref().unwrap().2, "chk_cli`ente ñ_saldo");
        assert!(c.parts[2].text.ends_with("DEFAULT 'x, y'"), "{}", c.parts[2].text);
        assert!(c.tail.contains("PARTITIONS 3"));
        assert!(parse("CREATE VIEW x AS SELECT 1").is_none());
    }

    #[test]
    fn rebuilds_under_the_new_name() {
        let mut renames = Vec::new();
        let d = ddl(&original(), &mut renames, "cli`ente ñ", "copia", "`copia`", &["id".into()], true, 64);
        // Collations, INVISIBLE, generated columns, options and partitions as printed.
        assert!(d.create.starts_with("CREATE TABLE `copia` (\n  `id` int NOT NULL AUTO_INCREMENT,"), "{}", d.create);
        assert!(d.create.contains("COLLATE utf8mb4_bin") && d.create.contains("latin1_spanish_ci") && d.create.contains("/*!80023 INVISIBLE */"));
        assert!(d.create.contains("ROW_FORMAT=COMPRESSED") && d.create.ends_with("PARTITIONS 3 */"));
        // The CHECK with a quote and a space in its name, renamed.
        assert!(d.create.contains("CONSTRAINT `chk_copia_saldo` CHECK ((`saldo` >= 0))"), "{}", d.create);
        assert!(!d.create.contains("KEY `"), "indexes go after the rows: {}", d.create);
        let ix = d.indexes.unwrap();
        assert!(ix.starts_with("ALTER TABLE `copia` ADD UNIQUE KEY `copia_ux_mail` (`e-mail`), ADD KEY `copia_fk_self`"), "{ix}");
        assert!(ix.contains("ADD KEY `copia_ix_fn` ((lower(`nombre`)))"), "{ix}");
        // A self reference points to the clone.
        assert_eq!(d.foreign_keys.unwrap(), "ALTER TABLE `copia` ADD CONSTRAINT `copia_fk_self` FOREIGN KEY (`otro`) REFERENCES `copia` (`id`)");
        assert_eq!(renames.len(), 4);
        // Without indexes: none, except one an AUTO_INCREMENT column leads.
        let mut r = Vec::new();
        let d = ddl(&original(), &mut r, "cli`ente ñ", "copia", "`copia`", &["otro".into()], false, 64);
        assert!(d.indexes.is_none());
        assert!(d.create.contains("KEY `copia_fk_self` (`otro`)"), "{}", d.create);
    }

    #[test]
    fn qualified_references_and_other_databases() {
        let c = parse(
            "CREATE TABLE `t` (\n  `a` int,\n  CONSTRAINT `f1` FOREIGN KEY (`a`) REFERENCES `db`.`t` (`id`),\n  CONSTRAINT `f2` FOREIGN KEY (`a`) REFERENCES `otra`.`t` (`id`)\n) ENGINE=InnoDB",
        )
        .unwrap();
        let o = Original { create: c, database: Some("db".into()), tidb: false };
        let d = ddl(&o, &mut Vec::new(), "t", "t2", "`t2`", &[], true, 64);
        let fk = d.foreign_keys.unwrap();
        assert!(fk.contains("REFERENCES `db`.`t2` (`id`)") && fk.contains("REFERENCES `otra`.`t` (`id`)"), "{fk}");
    }

    #[test]
    fn a_clone_that_lost_something_is_told_apart() {
        let o = original();
        let mut renames = Vec::new();
        let d = ddl(&o, &mut renames, "cli`ente ñ", "copia", "`copia`", &["id".into()], true, 64);
        // What the server would print for the clone: the same, renamed,
        // with other counters, indexes and keys in another order.
        let printed = |create: &str| {
            let mut s = create.replacen(") ENGINE=InnoDB AUTO_INCREMENT=7", ") ENGINE=InnoDB AUTO_INCREMENT=12", 1);
            let ix = d.indexes.as_ref().unwrap().trim_start_matches("ALTER TABLE `copia` ADD ").replace(", ADD ", ",\n  ");
            let fk = d.foreign_keys.as_ref().unwrap().trim_start_matches("ALTER TABLE `copia` ADD ").to_string();
            let at = s.find("\n) ENGINE").unwrap();
            s.insert_str(at, &format!(",\n  {fk},\n  {ix}"));
            s
        };
        let same = parse(&printed(&d.create)).unwrap();
        assert_eq!(differences(&o.create, &same, &renames, "cli`ente ñ", "copia", Some("db"), Stage::Done { with_indexes: true }), Vec::<String>::new());
        // A collation lost.
        let lost = parse(&printed(&d.create.replace(" CHARACTER SET utf8mb4 COLLATE utf8mb4_bin", ""))).unwrap();
        let diff = differences(&o.create, &lost, &renames, "cli`ente ñ", "copia", Some("db"), Stage::Created);
        assert!(diff.len() == 1 && diff[0].contains("utf8mb4_bin"), "{diff:?}");
        // A CHECK lost (TiDB with CHECKs off), INVISIBLE lost.
        let lost = parse(&printed(&d.create.replace(",\n  CONSTRAINT `chk_copia_saldo` CHECK ((`saldo` >= 0))", ""))).unwrap();
        let diff = differences(&o.create, &lost, &renames, "cli`ente ñ", "copia", Some("db"), Stage::Created);
        assert!(diff.iter().any(|d| d.contains("falta en el clon") && d.contains("CHECK")), "{diff:?}");
        let lost = parse(&printed(&d.create.replace(" /*!80023 INVISIBLE */", ""))).unwrap();
        assert!(!differences(&o.create, &lost, &renames, "cli`ente ñ", "copia", Some("db"), Stage::Created).is_empty());
        // Partitions lost.
        let lost = parse(&printed(&d.create.replace("\n/*!50100 PARTITION BY KEY (`id`)\nPARTITIONS 3 */", ""))).unwrap();
        let diff = differences(&o.create, &lost, &renames, "cli`ente ñ", "copia", Some("db"), Stage::Created);
        assert!(diff.iter().any(|d| d.contains("opciones de la tabla")), "{diff:?}");
    }

    #[test]
    fn tidb_constraints_without_indentation() {
        let c = parse("CREATE TABLE `ck` (\n  `id` int(11) NOT NULL AUTO_INCREMENT,\n  `saldo` int(11) DEFAULT NULL,\n  PRIMARY KEY (`id`) /*T![clustered_index] CLUSTERED */,\nCONSTRAINT `chk_cli``ente ñ_saldo` CHECK ((`saldo` >= 0)),\nCONSTRAINT `chk_simple` CHECK ((`saldo` < 1000))\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin AUTO_INCREMENT=30001").unwrap();
        let kinds: Vec<Kind> = c.parts.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, vec![Kind::Column, Kind::Column, Kind::Primary, Kind::Check, Kind::Check]);
        assert_eq!(c.parts[3].name.as_ref().unwrap().2, "chk_cli`ente ñ_saldo");
    }

    #[test]
    fn tidb_checks_lose_one_level_of_parentheses() {
        assert_eq!(unwrap_check("CONSTRAINT `c` CHECK ((`saldo` >= 0))"), "CONSTRAINT `c` CHECK (`saldo` >= 0)");
        assert_eq!(unwrap_check("CONSTRAINT `c` CHECK ((`a` > 0) OR (`b` > 0))"), "CONSTRAINT `c` CHECK ((`a` > 0) OR (`b` > 0))");
        assert_eq!(unwrap_check("CONSTRAINT `c` CHECK ((`a` = ')')) /*T![check_constraint] NOT ENFORCED */"), "CONSTRAINT `c` CHECK (`a` = ')') /*T![check_constraint] NOT ENFORCED */");
        assert_eq!(unwrap_check("CONSTRAINT `c` CHECK (`a` > 0)"), "CONSTRAINT `c` CHECK (`a` > 0)");
    }

    #[test]
    fn counters_are_not_compared() {
        assert_eq!(
            without_counters(") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin /*T![auto_rand_base] AUTO_RANDOM_BASE=30001 */"),
            without_counters(") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin /*T![auto_rand_base] AUTO_RANDOM_BASE=90001 */")
        );
        assert_eq!(without_counters(") ENGINE=InnoDB AUTO_INCREMENT=5 COMMENT='a'"), without_counters(") ENGINE=InnoDB AUTO_INCREMENT=9 COMMENT='a'"));
        assert_ne!(without_counters(") ENGINE=InnoDB ROW_FORMAT=COMPRESSED"), without_counters(") ENGINE=InnoDB"));
    }

    struct D(dbine_driver::DriverInfo);

    #[dbine_driver::async_trait]
    impl Driver for D {
        fn info(&self) -> &dbine_driver::DriverInfo {
            &self.0
        }
        async fn connect(&self, _: &dbine_driver::ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
            Err(Error::Unsupported("test".into()))
        }
    }

    fn driver(id: &'static str, dialect: &'static str) -> D {
        D(dbine_driver::DriverInfo {
            id,
            name: id,
            family: dbine_driver::Family::Relational,
            language: dbine_driver::Language::Sql,
            dialect,
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: false,
            object_kinds: vec![dbine_driver::ObjectKindInfo::tables()],
        })
    }

    fn table(name: &str) -> dbine_driver::TableSchema {
        dbine_driver::TableSchema {
            kind: "table".into(),
            name: name.into(),
            columns: vec![dbine_driver::ColumnDef { name: "a".into(), data_type: "int".into(), ..Default::default() }],
            indexes: vec![dbine_driver::IndexDef { name: "ix_fn".into(), columns: vec!["(lower(`a`))".into()], ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn names_count_characters_and_fit_the_engine() {
        let at = chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap().and_hms_opt(7, 5, 9).unwrap();
        let my = driver("mysql", "mysql");
        // 64 characters with a ñ (65 bytes): MySQL takes it.
        let n = format!("ñ{}", "x".repeat(63));
        assert!(super::super::plan_clone(&my, &table("t"), &n).is_ok());
        let e = super::super::plan_clone(&my, &table("t"), &format!("{n}x")).unwrap_err().to_string();
        assert!(e.contains("64 caracteres"), "{e}");
        // The proposed name is cut before the suffix, never refused.
        let long = "c".repeat(60);
        let p = super::super::default_clone_name_for(&my, &long, at);
        assert_eq!(p.chars().count(), 64);
        assert!(p.ends_with("_20260930_070509") && super::super::plan_clone(&my, &table(&long), &p).is_ok(), "{p}");
        assert_eq!(super::super::default_clone_name_for(&my, "clientes", at), "clientes_20260930_070509");
        // PostgreSQL counts bytes.
        let pg = driver("postgres", "postgres");
        let p = super::super::default_clone_name_for(&pg, &"ñ".repeat(40), at);
        assert!(p.len() <= 63 && super::super::plan_clone(&pg, &table("t"), &p).is_ok(), "{p}");
    }

    #[test]
    fn tidb_leaves_room_for_the_hidden_column() {
        let ti = driver("tidb", "mysql");
        let p = super::super::plan_clone(&ti, &table("t"), &"n".repeat(64)).unwrap();
        let ix = &p.table.indexes[0].name;
        // `_V$_<index>_0` within 64.
        assert!(format!("_V$_{ix}_0").len() <= 64, "{ix}");
    }

    #[test]
    fn tidb_check_names_fit_in_64_bytes() {
        // TiDB takes 64 characters for tables and indexes but only 64 bytes
        // for a CHECK's name: `ááá…á_ck_x` (55 characters, 105 bytes) failed.
        let old = "ñ".repeat(20);
        let new = "á".repeat(50);
        let mut t = table(&old);
        t.checks.push(dbine_driver::CheckDef { name: Some("ck_x".into()), expression: "a > 0".into() });
        let ti = driver("tidb", "mysql");
        let p = super::super::plan_clone(&ti, &t, &new).unwrap();
        let ck = p.table.checks[0].name.clone().unwrap();
        assert!(ck.len() <= 64 && ck.chars().count() <= 56, "{ck}");
        assert!(p.table.indexes.iter().all(|i| i.name.len() <= 64), "{:?}", p.table.indexes);
        assert!(p.notes.iter().any(|n| n.contains("56 caracteres y 64 bytes")), "{:?}", p.notes);
        // The same from SHOW CREATE TABLE (names the catalog didn't report).
        let c = parse(&format!("CREATE TABLE `{old}` (\n  `id` int NOT NULL,\n  `a` int,\n  PRIMARY KEY (`id`),\n  KEY `ix_a` (`a`),\n  CONSTRAINT `ck_x` CHECK ((`a` > 0))\n) ENGINE=InnoDB")).unwrap();
        let o = Original { create: c, database: None, tidb: true };
        let mut renames = Vec::new();
        let d = ddl(&o, &mut renames, &old, &new, &format!("`{new}`"), &[], true, 56);
        assert!(renames.iter().all(|r| r.to.len() <= 64 && r.to.chars().count() <= 56), "{renames:?}");
        assert!(d.notes.iter().all(|n| n.contains("56 caracteres y 64 bytes")), "{:?}", d.notes);
        // MySQL and MariaDB: characters only, as before.
        let my = driver("mysql", "mysql");
        let p = super::super::plan_clone(&my, &t, &new).unwrap();
        assert_eq!(p.table.checks[0].name.as_deref(), Some(format!("{new}_ck_x").as_str()));
    }

    /// TiDB prints a newline inside a string literal as it is.
    const TIDB_NL: &str = "CREATE TABLE `adv2` (\n  `id` int(11) NOT NULL,\n  `a` varchar(20) DEFAULT NULL,\n  `g` varchar(40) GENERATED ALWAYS AS (concat(`a`, _utf8mb4',\nx')) STORED,\n  `h` varchar(9) DEFAULT 'p\n)q,',\n  PRIMARY KEY (`id`) /*T![clustered_index] CLUSTERED */,\nCONSTRAINT `ck_nl` CHECK ((`a` != _utf8mb4'q,\nr'))\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin";

    #[test]
    fn tidb_newlines_inside_literals_are_kept() {
        let c = parse(TIDB_NL).unwrap();
        let kinds: Vec<Kind> = c.parts.iter().map(|p| p.kind).collect();
        assert_eq!(kinds, vec![Kind::Column, Kind::Column, Kind::Column, Kind::Column, Kind::Primary, Kind::Check]);
        assert!(c.parts[2].text.ends_with("_utf8mb4',\nx')) STORED"), "{:?}", c.parts[2].text);
        assert!(c.parts[3].text.ends_with("DEFAULT 'p\n)q,'"), "{:?}", c.parts[3].text);
        assert_eq!(c.parts[5].text, "CONSTRAINT `ck_nl` CHECK ((`a` != _utf8mb4'q,\nr'))");
        let o = Original { create: c, database: Some("db".into()), tidb: true };
        let mut renames = Vec::new();
        let d = ddl(&o, &mut renames, "adv2", "adv2_c", "`adv2_c`", &[], true, 56);
        // The literals go exactly as printed, nothing added inside them.
        assert!(d.create.contains("_utf8mb4',\nx')) STORED,\n  `h`"), "{}", d.create);
        assert!(d.create.contains("CONSTRAINT `adv2_c_ck_nl` CHECK (`a` != _utf8mb4'q,\nr')"), "{}", d.create);
        // What TiDB prints for a faithful clone: equal.
        let same = parse(&TIDB_NL.replace("`adv2`", "`adv2_c`").replace("`ck_nl`", "`adv2_c_ck_nl`")).unwrap();
        assert_eq!(differences(&o.create, &same, &renames, "adv2", "adv2_c", Some("db"), Stage::Created), Vec::<String>::new());
        // Spaces added inside the literal: told apart.
        let changed = parse(&TIDB_NL.replace("`adv2`", "`adv2_c`").replace("`ck_nl`", "`adv2_c_ck_nl`").replace(",\nx'", ",\n  x'")).unwrap();
        assert!(!differences(&o.create, &changed, &renames, "adv2", "adv2_c", Some("db"), Stage::Created).is_empty());
        // A literal that never closes: not taken apart.
        assert!(parse("CREATE TABLE `t` (\n  `a` int DEFAULT 'x,\n) ENGINE=InnoDB").is_none());
    }

    #[test]
    fn escaped_quotes_inside_literals() {
        assert_eq!(unwrap_check("CONSTRAINT `c` CHECK ((`a` <> 'x\\\\')) /*T![check_constraint] NOT ENFORCED */"), "CONSTRAINT `c` CHECK (`a` <> 'x\\\\') /*T![check_constraint] NOT ENFORCED */");
        assert_eq!(unwrap_check("CONSTRAINT `c` CHECK ((`a` <> 'x\\')')) "), "CONSTRAINT `c` CHECK (`a` <> 'x\\')') ");
        assert_eq!(find_outside("'a\\' REFERENCES ' REFERENCES x", 0, " REFERENCES "), Some(17));
    }

    #[test]
    fn one_fulltext_index_per_alter() {
        let c = parse("CREATE TABLE `adv3` (\n  `id` int NOT NULL,\n  `a` text,\n  `b` text,\n  PRIMARY KEY (`id`),\n  KEY `ix_id` (`id`,`a`(10)),\n  FULLTEXT KEY `ft1` (`a`),\n  FULLTEXT KEY `ft2` (`b`)\n) ENGINE=InnoDB").unwrap();
        let o = Original { create: c, database: None, tidb: false };
        let d = ddl(&o, &mut Vec::new(), "adv3", "c", "`c`", &[], true, 64);
        assert_eq!(
            d.indexes.unwrap(),
            "ALTER TABLE `c` ADD KEY `c_ix_id` (`id`,`a`(10));\nALTER TABLE `c` ADD FULLTEXT KEY `c_ft1` (`a`);\nALTER TABLE `c` ADD FULLTEXT KEY `c_ft2` (`b`)"
        );
        // Just one: the single ALTER, as before.
        let c = parse("CREATE TABLE `uno` (\n  `a` text,\n  KEY `k` (`a`(5)),\n  FULLTEXT KEY `ft1` (`a`)\n) ENGINE=InnoDB").unwrap();
        let o = Original { create: c, database: None, tidb: false };
        let d = ddl(&o, &mut Vec::new(), "uno", "c", "`c`", &[], true, 64);
        assert_eq!(d.indexes.unwrap(), "ALTER TABLE `c` ADD KEY `c_k` (`a`(5)), ADD FULLTEXT KEY `c_ft1` (`a`)");
    }

    #[test]
    fn shortened_names_count_characters() {
        let long = "ñ".repeat(40);
        let c = parse(&format!("CREATE TABLE `t` (\n  `a` int,\n  KEY `ix_{long}` (`a`)\n) ENGINE=InnoDB")).unwrap();
        let o = Original { create: c, database: None, tidb: false };
        let mut renames = Vec::new();
        let d = ddl(&o, &mut renames, "t", &"n".repeat(30), "`n`", &[], true, 64);
        assert!(d.notes.iter().all(|n| n.contains("64 caracteres")) && !d.notes.is_empty(), "{:?}", d.notes);
        assert_eq!(renames[0].to.chars().count(), 64, "{}", renames[0].to);
        let p = super::super::plan_clone(&driver("mysql", "mysql"), &table("t"), &"n".repeat(60)).unwrap();
        assert!(p.notes.iter().any(|n| n.contains("64 caracteres")), "{:?}", p.notes);
    }

    #[test]
    fn system_versioning_is_refused() {
        let c = parse("CREATE TABLE `sv` (\n  `id` int(11) NOT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB WITH SYSTEM VERSIONING").unwrap();
        let e = refuse(&c).unwrap_err().to_string();
        assert!(e.contains("versionado de sistema"), "{e}");
        assert!(refuse(&parse(MYSQL).unwrap()).is_ok());
    }
}
