//! Table structure from `sqlite_master` and the table-valued pragmas, the
//! table designer and the DDL.

use dbine_driver::ddl::{self, AutoIncrement, SqlFlavor};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{
    kinds, CheckDef, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, ForeignKeyDef, IndexDef,
    KeyDef, TableSchema,
};
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;

pub const FLAVOR: SqlFlavor = SqlFlavor {
    quote: Quote::Double,
    auto_increment: AutoIncrement::SqliteAutoincrement,
    comment_on: false,
    inline_comments: false,
    if_exists: true,
    // No ALTER TABLE … ADD CONSTRAINT: foreign keys live in CREATE TABLE
    // (they may point at tables created later; SQLite checks them on DML).
    fk_inline: true,
    multi_row_insert: true,
    true_literal: "TRUE",
    false_literal: "FALSE",
};

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        table_options: vec![
            Field::new("without_rowid", "WITHOUT ROWID", FieldKind::Bool).help("Tabla sin rowid: requiere clave primaria."),
            Field::new("strict", "STRICT", FieldKind::Bool).help("Tipos estrictos (SQLite 3.37 o posterior)."),
        ],
        ..DesignerSpec::sql_table(vec![
            "INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB", "BOOLEAN", "DATE", "DATETIME", "VARCHAR(255)", "DECIMAL(18,2)",
        ])
    }
}

pub fn create_templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::VIEW,
            label: "Nueva vista",
            template: "CREATE VIEW IF NOT EXISTS \"{name}\" AS\nSELECT\n    t.id,\n    t.nombre\nFROM \"tabla\" AS t\nWHERE t.activo = 1;\n".into(),
        },
        CreateTemplate {
            kind: kinds::TRIGGER,
            label: "Nuevo trigger",
            template: "CREATE TRIGGER IF NOT EXISTS \"{name}\"\nAFTER UPDATE ON \"tabla\"\nFOR EACH ROW\nBEGIN\n    UPDATE \"tabla\" SET modificado = CURRENT_TIMESTAMP WHERE id = NEW.id;\nEND;\n".into(),
        },
    ]
}

fn on(t: &TableSchema, k: &str) -> bool {
    t.options.get(k).is_some_and(|v| v == "true" || v == "1")
}

/// Index option: the descending key columns, as a list.
pub const DESC: &str = "desc";
/// Index option prefix: `collate:<column>` is a key column's collation when
/// it isn't `BINARY`.
pub const COLLATE: &str = "collate:";

/// The items of a list option (`a, (b + 1)`): split where the top level has
/// a comma.
fn option_list(ix: &IndexDef, key: &str) -> Vec<String> {
    match ix.options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
        Some(v) => paren_items(&format!("({v})"), 0).0,
        None => Vec::new(),
    }
}

/// Object kind: a virtual table (FTS5, FTS4, R*Tree…), compared by its
/// `CREATE VIRTUAL TABLE … USING module(…)`. Its shadow tables are the
/// module's business: neither one is a table of `database_schema`.
pub const VIRTUAL_TABLE: &str = "virtual_table";

pub fn virtual_tables() -> dbine_driver::ObjectKindInfo {
    dbine_driver::ObjectKindInfo::new(VIRTUAL_TABLE, "Tablas virtuales", true, true, true)
}

/// Is this `sqlite_master.sql` a virtual table's?
pub fn is_virtual(sql: &str) -> bool {
    sql.trim_start().get(..14).is_some_and(|h| h.eq_ignore_ascii_case("CREATE VIRTUAL"))
}

/// An index column: a name, or an expression kept in parentheses.
fn index_column(c: &str) -> String {
    if c.starts_with('(') && c.ends_with(')') {
        c.to_string()
    } else {
        quote_ident(Quote::Double, c)
    }
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let mut out = Vec::new();
    if parts.drop || parts.create {
        let mut s = ddl::table_ddl(&FLAVOR, t, DdlParts { drop: parts.drop, if_exists: parts.if_exists, create: parts.create, ..Default::default() });
        let opts: Vec<&str> = [("without_rowid", "WITHOUT ROWID"), ("strict", "STRICT")]
            .iter()
            .filter(|(k, _)| parts.create && on(t, k))
            .map(|(_, v)| *v)
            .collect();
        if !opts.is_empty() {
            s = s.replacen("\n);", &format!("\n) {};", opts.join(", ")), 1);
        }
        out.push(s);
    }
    if parts.indexes {
        let table = quote_ident(Quote::Double, &t.name);
        for ix in &t.indexes {
            let desc = option_list(ix, DESC);
            let cols: Vec<String> = ix
                .columns
                .iter()
                .map(|c| {
                    let mut s = index_column(c);
                    if let Some(coll) = ix.options.get(&format!("{COLLATE}{c}")).filter(|v| !v.is_empty()) {
                        s.push_str(&format!(" COLLATE {coll}"));
                    }
                    if desc.contains(c) {
                        s.push_str(" DESC");
                    }
                    s
                })
                .collect();
            let mut s = format!(
                "CREATE {}INDEX {}{} ON {table} ({})",
                if ix.unique { "UNIQUE " } else { "" },
                if parts.if_exists { "IF NOT EXISTS " } else { "" },
                quote_ident(Quote::Double, &ix.name),
                cols.join(", ")
            );
            if let Some(w) = ix.filter.as_deref().filter(|w| !w.is_empty()) {
                s.push_str(&format!(" WHERE {w}"));
            }
            s.push(';');
            out.push(s);
        }
    }
    // parts.foreign_keys: already inside CREATE TABLE.
    out.join("\n")
}

// --- Schema sync ----------------------------------------------------------

/// The same CHECKs on both sides (by name and condition).
fn same_checks(a: &[CheckDef], b: &[CheckDef]) -> bool {
    use dbine_driver::alter::check_expr;
    let key = |v: &[CheckDef]| {
        let mut k: Vec<(String, String)> = v.iter().map(|c| (c.name.clone().unwrap_or_default().to_lowercase(), check_expr(&c.expression))).collect();
        k.sort();
        k
    };
    key(a) == key(b)
}

/// Columns are added in place; any other change rebuilds the table (new
/// table, copy, drop, rename). SQLite can't add or drop a CHECK with ALTER
/// TABLE, so a table whose CHECKs change is rebuilt too.
pub fn sync_script(changes: &[dbine_driver::TableChange]) -> dbine_driver::Result<dbine_driver::SyncScript> {
    use dbine_driver::alter::{self, AlterStyle, ColumnAlter, TableChange};
    let changes: Vec<TableChange> = changes
        .iter()
        .map(|ch| match ch {
            TableChange::Alter { old, new } if !same_checks(&old.checks, &new.checks) => {
                // A foreign key the new table can't have makes the planner
                // rebuild it (the rebuild doesn't read the old table's keys).
                let mut old = old.clone();
                old.foreign_keys.push(ForeignKeyDef { columns: vec!["\u{0}".into()], ref_table: "\u{0}rebuild".into(), ..Default::default() });
                TableChange::Alter { old, new: new.clone() }
            }
            other => other.clone(),
        })
        .collect();
    let cd = |t: &TableSchema, c: &ColumnDef| ddl::column_def(&FLAVOR, t, c);
    let dd = |t: &TableSchema, p: DdlParts| Ok(table_ddl(t, p));
    alter::sync_script(&AlterStyle::from_flavor(&FLAVOR, ColumnAlter::Recreate, &cd, &dd), &changes)
}

// --- Catalog --------------------------------------------------------------

/// Ordinary tables: not SQLite's own, not virtual tables nor their shadow
/// tables (FTS3/4/5 and R*Tree name them `<table>_<suffix>`).
const USER_TABLES: &str = "m.type = 'table' AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
  AND COALESCE(m.sql, '') NOT LIKE 'CREATE VIRTUAL %'
  AND NOT EXISTS (SELECT 1 FROM sqlite_master v
                   WHERE v.type = 'table' AND v.sql LIKE 'CREATE VIRTUAL %' AND substr(m.name, 1, length(v.name) + 1) = v.name || '_'
                     AND substr(m.name, length(v.name) + 2) IN ('data', 'idx', 'content', 'docsize', 'config', 'segments', 'segdir', 'stat', 'node', 'rowid', 'parent'))";

/// `NO ACTION` is the default.
fn action(a: String) -> Option<String> {
    (!a.eq_ignore_ascii_case("NO ACTION")).then_some(a)
}

/// The text after the column list's closing parenthesis, upper-cased
/// (`WITHOUT ROWID`, `STRICT`).
fn trailer(create: &str) -> String {
    create.rfind(')').map(|i| create[i + 1..].to_ascii_uppercase()).unwrap_or_default()
}

/// Top-level items of the parenthesised list that starts at `open`.
fn paren_items(s: &str, open: usize) -> (Vec<String>, usize) {
    let (mut depth, mut items, mut cur, mut quote) = (0, Vec::new(), String::new(), None::<char>);
    for (i, ch) in s[open..].char_indices() {
        if let Some(q) = quote {
            cur.push(ch);
            if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' | '[' => {
                quote = Some(if ch == '[' { ']' } else { ch });
                cur.push(ch);
            }
            '(' => {
                depth += 1;
                if depth > 1 {
                    cur.push(ch);
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    items.push(cur.trim().to_string());
                    return (items, open + i + 1);
                }
                cur.push(ch);
            }
            ',' if depth == 1 => items.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(ch),
        }
    }
    (items, s.len())
}

/// Column list and WHERE predicate of a `CREATE INDEX` statement.
pub fn parse_index_sql(sql: &str) -> (Vec<String>, Option<String>) {
    let upper = sql.to_ascii_uppercase();
    let Some(on) = upper.find(" ON ") else { return (Vec::new(), None) };
    let Some(open) = sql[on..].find('(').map(|i| on + i) else { return (Vec::new(), None) };
    let (items, end) = paren_items(sql, open);
    let rest = sql[end..].trim();
    let filter = (rest.len() > 5 && rest[..5].eq_ignore_ascii_case("WHERE")).then(|| rest[5..].trim().trim_end_matches(';').trim().to_string());
    (items, filter)
}

/// An index key as written, without its `COLLATE x` and `ASC` / `DESC`
/// (the catalog reports those apart).
fn key_expression(item: &str) -> String {
    let mut e = item.trim().to_string();
    for kw in [" DESC", " ASC"] {
        if e.len() > kw.len() && e[e.len() - kw.len()..].eq_ignore_ascii_case(kw) {
            e.truncate(e.len() - kw.len());
            e = e.trim_end().to_string();
            break;
        }
    }
    if let Some(p) = e.to_ascii_uppercase().rfind(" COLLATE ") {
        let rest = e[p + 9..].trim();
        if !rest.is_empty() && !rest.contains(|c: char| c == ')' || c.is_whitespace()) {
            e.truncate(p);
        }
    }
    e.trim().to_string()
}

/// A top-level piece of a column or constraint definition.
enum Tok {
    /// A keyword or a name; `quoted` names never read as keywords.
    Word { text: String, quoted: bool },
    /// What a parenthesised group holds.
    Group(String),
}

/// The pieces of one item of CREATE TABLE's list (`a INT CHECK (a > 0)`).
fn tokens(item: &str) -> Vec<Tok> {
    let chars: Vec<(usize, char)> = item.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (pos, ch) = chars[i];
        match ch {
            c if c.is_whitespace() || c == ',' => i += 1,
            '-' if item[pos..].starts_with("--") => {
                i = chars.iter().position(|(p, c)| *p > pos && *c == '\n').unwrap_or(chars.len());
            }
            '/' if item[pos..].starts_with("/*") => {
                let end = item[pos + 2..].find("*/").map_or(item.len(), |e| pos + 2 + e + 2);
                i = chars.iter().position(|(p, _)| *p >= end).unwrap_or(chars.len());
            }
            '(' => {
                let (_, end) = paren_items(item, pos);
                // The group's text as written (commas included), without its parentheses.
                let inner_end = if end > pos + 1 && item[..end].ends_with(')') { end - 1 } else { end };
                out.push(Tok::Group(item[pos + 1..inner_end].trim().to_string()));
                i = chars.iter().position(|(p, _)| *p >= end).unwrap_or(chars.len());
            }
            '"' | '`' | '[' | '\'' => {
                let close = if ch == '[' { ']' } else { ch };
                let mut j = i + 1;
                let mut text = String::new();
                while j < chars.len() {
                    if chars[j].1 == close {
                        // A doubled quote is one quote character.
                        if close != ']' && j + 1 < chars.len() && chars[j + 1].1 == close {
                            text.push(close);
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    text.push(chars[j].1);
                    j += 1;
                }
                out.push(Tok::Word { text, quoted: true });
                i = j + 1;
            }
            _ => {
                let mut j = i;
                let mut text = String::new();
                while j < chars.len() && !chars[j].1.is_whitespace() && !matches!(chars[j].1, '(' | ',' | '"' | '`' | '[' | '\'') {
                    text.push(chars[j].1);
                    j += 1;
                }
                out.push(Tok::Word { text, quoted: false });
                i = j;
            }
        }
    }
    out
}

fn keyword(t: Option<&Tok>, kw: &str) -> bool {
    matches!(t, Some(Tok::Word { text, quoted: false }) if text.eq_ignore_ascii_case(kw))
}

/// The CHECK constraints of a `CREATE TABLE` statement, the table's and the
/// columns' alike (a column's CHECK becomes a table constraint when written
/// back: SQLite enforces both the same way). `CONSTRAINT name` before a
/// CHECK names it.
pub fn parse_checks(create: &str) -> Vec<CheckDef> {
    // The column list: the first parenthesis outside quotes.
    let Some(open) = tokens_start(create) else { return Vec::new() };
    let (items, _) = paren_items(create, open);
    let mut out = Vec::new();
    for item in items {
        let toks = tokens(&item);
        for (i, t) in toks.iter().enumerate() {
            if !keyword(Some(t), "CHECK") {
                continue;
            }
            let Some(Tok::Group(expr)) = toks.get(i + 1) else { continue };
            let name = match (i >= 2).then(|| (&toks[i - 2], &toks[i - 1])) {
                Some((c, Tok::Word { text, .. })) if keyword(Some(c), "CONSTRAINT") => Some(text.clone()),
                _ => None,
            };
            out.push(CheckDef { name, expression: expr.clone() });
        }
    }
    out
}

/// Where a `CREATE TABLE`'s column list opens: the first `(` outside quotes.
fn tokens_start(create: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, ch) in create.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None => match ch {
                '"' | '`' | '\'' => quote = Some(ch),
                '[' => quote = Some(']'),
                '(' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// Rows of a catalog query as JSON cells (how the libSQL driver gets them
/// over HTTP, and how [`read_schema`] adapts rusqlite's).
pub type Rows = Vec<Vec<Value>>;

/// Rows of `sql` on a rusqlite connection, as JSON cells.
pub fn query_rows(c: &Connection, sql: &str) -> rusqlite::Result<Rows> {
    let mut stmt = c.prepare(sql)?;
    let n = stmt.column_count();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(
            (0..n)
                .map(|i| match r.get_ref_unwrap(i) {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(v) => v.into(),
                    ValueRef::Real(v) => serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number),
                    ValueRef::Text(t) | ValueRef::Blob(t) => String::from_utf8_lossy(t).into_owned().into(),
                })
                .collect(),
        );
    }
    Ok(out)
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn opt_text(v: &Value) -> Option<String> {
    (!v.is_null()).then(|| text(v))
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        Value::Bool(b) => *b as i64,
        _ => 0,
    }
}

pub fn read_schema(c: &Connection) -> rusqlite::Result<Vec<TableSchema>> {
    read_schema_with(&mut |sql| query_rows(c, sql))
}

/// Tables with columns, keys, foreign keys and indexes, from catalog
/// queries run by `q` (a local connection or libSQL over HTTP).
pub fn read_schema_with<E>(q: &mut dyn FnMut(&str) -> Result<Rows, E>) -> Result<Vec<TableSchema>, E> {
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, TableSchema> = BTreeMap::new();
    let mut autoinc: BTreeMap<String, bool> = BTreeMap::new();
    for r in q(&format!("SELECT m.name, m.sql FROM sqlite_master m WHERE {USER_TABLES} ORDER BY m.name"))? {
        let (name, sql) = (text(&r[0]), text(&r[1]));
        let mut options = BTreeMap::new();
        let tail = trailer(&sql);
        if tail.contains("WITHOUT ROWID") {
            options.insert("without_rowid".to_string(), "true".to_string());
        }
        if tail.contains("STRICT") {
            options.insert("strict".to_string(), "true".to_string());
        }
        autoinc.insert(name.clone(), sql.to_ascii_uppercase().contains("AUTOINCREMENT"));
        order.push(name.clone());
        let checks = parse_checks(&sql);
        map.insert(name.clone(), TableSchema { kind: kinds::TABLE.into(), name, options, checks, ..Default::default() });
    }

    // Columns (hidden and generated ones left out) and the primary key.
    let rows = q(&format!(
        "SELECT m.name, p.name, p.type, p.\"notnull\", p.dflt_value, p.pk
         FROM sqlite_master m, pragma_table_xinfo(m.name) p
         WHERE {USER_TABLES} AND p.hidden = 0
         ORDER BY m.name, p.cid"
    ))?;
    let mut pk_cols: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
    for r in rows {
        let (table, name, data_type) = (text(&r[0]), text(&r[1]), text(&r[2]));
        let (notnull, default_value, pk) = (int(&r[3]) != 0, opt_text(&r[4]), int(&r[5]));
        let Some(t) = map.get_mut(&table) else { continue };
        if pk > 0 {
            pk_cols.entry(table.clone()).or_default().push((pk, name.clone()));
        }
        t.columns.push(ColumnDef { name, data_type, nullable: !notnull, default_value, ..Default::default() });
    }
    for (table, mut cols) in pk_cols {
        let Some(t) = map.get_mut(&table) else { continue };
        cols.sort();
        let columns: Vec<String> = cols.into_iter().map(|c| c.1).collect();
        // A key column never holds NULL in practice (the pragma says it may).
        for c in t.columns.iter_mut().filter(|c| columns.contains(&c.name)) {
            c.nullable = false;
        }
        if columns.len() == 1 && autoinc.get(&table).copied().unwrap_or(false) {
            if let Some(c) = t.columns.iter_mut().find(|c| c.name == columns[0] && c.data_type.eq_ignore_ascii_case("INTEGER")) {
                c.auto_increment = true;
            }
        }
        t.primary_key = Some(KeyDef { name: None, columns });
    }

    // Foreign keys (unnamed in the pragma); `to` is empty when they point
    // at the referenced table's primary key.
    let rows = q(&format!(
        "SELECT m.name, f.id, f.\"table\", f.\"from\", f.\"to\", f.on_update, f.on_delete
         FROM sqlite_master m, pragma_foreign_key_list(m.name) f
         WHERE {USER_TABLES}
         ORDER BY m.name, f.id, f.seq"
    ))?;
    let mut fks: Vec<(String, i64, ForeignKeyDef)> = Vec::new();
    for r in rows {
        let (table, id, ref_table, from) = (text(&r[0]), int(&r[1]), text(&r[2]), text(&r[3]));
        let (to, on_update, on_delete) = (opt_text(&r[4]), text(&r[5]), text(&r[6]));
        match fks.last_mut() {
            Some((t, i, fk)) if *t == table && *i == id => {
                fk.columns.push(from);
                fk.ref_columns.extend(to);
            }
            _ => fks.push((
                table,
                id,
                ForeignKeyDef {
                    name: None,
                    columns: vec![from],
                    ref_schema: None,
                    ref_table,
                    ref_columns: to.into_iter().collect(),
                    on_delete: action(on_delete),
                    on_update: action(on_update),
                },
            )),
        }
    }
    for (table, _, mut fk) in fks {
        if fk.ref_columns.is_empty() {
            fk.ref_columns = map.get(&fk.ref_table).and_then(|t| t.primary_key.as_ref()).map(|k| k.columns.clone()).unwrap_or_default();
        }
        if let Some(t) = map.get_mut(&table) {
            t.foreign_keys.push(fk);
        }
    }
    // The pragma numbers them in reverse creation order: a stable order instead.
    for t in map.values_mut() {
        t.foreign_keys.sort_by(|a, b| a.columns.cmp(&b.columns));
    }

    // Indexes other than the primary key; UNIQUE constraints' automatic
    // indexes get a name that CREATE INDEX accepts.
    let rows = q(&format!(
        "SELECT m.name, il.name, il.\"unique\", il.origin, ii.name, s.sql, ii.\"desc\", ii.coll
         FROM sqlite_master m, pragma_index_list(m.name) il, pragma_index_xinfo(il.name) ii
         LEFT JOIN sqlite_master s ON s.type = 'index' AND s.name = il.name
         WHERE {USER_TABLES} AND il.origin <> 'pk' AND ii.key = 1
         ORDER BY m.name, il.name, ii.seqno"
    ))?;
    for r in rows {
        let (table, index, unique, origin) = (text(&r[0]), text(&r[1]), int(&r[2]) != 0, text(&r[3]));
        let (column, sql) = (opt_text(&r[4]), opt_text(&r[5]));
        let Some(t) = map.get_mut(&table) else { continue };
        let name = if origin == "u" { format!("{table}_{}_key", t.indexes.len() + 1) } else { index.clone() };
        let pos = match t.indexes.last() {
            Some(ix) if ix.kind.as_deref() == Some(index.as_str()) => ix.columns.len(),
            _ => {
                let filter = sql.as_deref().and_then(|s| parse_index_sql(s).1);
                // `kind` carries the catalog name while building; cleared below.
                t.indexes.push(IndexDef { name, columns: Vec::new(), unique, kind: Some(index.clone()), filter, ..Default::default() });
                0
            }
        };
        let ix = t.indexes.last_mut().expect("pushed");
        let col = match column {
            Some(c) => c,
            None => {
                let parsed = sql.as_deref().map(|s| parse_index_sql(s).0).unwrap_or_default();
                let expr = parsed.get(pos).map(|e| key_expression(e)).unwrap_or_default();
                if expr.starts_with('(') && expr.ends_with(')') { expr } else { format!("({expr})") }
            }
        };
        if int(&r[6]) != 0 {
            let list = ix.options.entry(DESC.to_string()).or_default();
            if !list.is_empty() {
                list.push_str(", ");
            }
            list.push_str(&col);
        }
        let coll = text(&r[7]);
        if !coll.is_empty() && !coll.eq_ignore_ascii_case("BINARY") {
            ix.options.insert(format!("{COLLATE}{col}"), coll);
        }
        ix.columns.push(col);
    }
    let mut out: Vec<TableSchema> = order.iter().filter_map(|n| map.remove(n)).collect();
    for t in &mut out {
        for ix in &mut t.indexes {
            ix.kind = None;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQL: &str = r#"
        CREATE TABLE clientes (id INTEGER PRIMARY KEY AUTOINCREMENT, email TEXT NOT NULL UNIQUE, nombre TEXT DEFAULT 'x');
        CREATE TABLE pedidos (
            id INTEGER PRIMARY KEY,
            cliente_id INTEGER NOT NULL REFERENCES clientes ON DELETE CASCADE,
            estado TEXT,
            total NUMERIC NOT NULL DEFAULT 0
        );
        CREATE TABLE items (
            pedido_id INTEGER NOT NULL, linea INTEGER NOT NULL, cliente_id INTEGER, producto TEXT,
            PRIMARY KEY (pedido_id, linea),
            FOREIGN KEY (pedido_id) REFERENCES pedidos (id) ON DELETE CASCADE,
            FOREIGN KEY (cliente_id) REFERENCES clientes (id) ON DELETE SET NULL
        ) WITHOUT ROWID;
        CREATE UNIQUE INDEX ux_email ON clientes (lower(email));
        CREATE INDEX ix_estado ON pedidos (estado, total) WHERE estado IS NOT NULL;
    "#;

    #[test]
    fn schema_is_read_and_rebuilt() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SQL).unwrap();
        let s = read_schema(&c).unwrap();
        assert_eq!(s.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["clientes", "items", "pedidos"]);
        let clientes = &s[0];
        assert!(clientes.columns[0].auto_increment);
        assert_eq!(clientes.columns[2].default_value.as_deref(), Some("'x'"));
        assert!(clientes.indexes.iter().any(|i| i.name == "clientes_1_key" && i.unique && i.columns == ["email"]), "{:?}", clientes.indexes);
        assert!(clientes.indexes.iter().any(|i| i.name == "ux_email" && i.columns == ["(lower(email))"]));
        let items = &s[1];
        assert_eq!(items.options.get("without_rowid").map(String::as_str), Some("true"));
        assert_eq!(items.primary_key.as_ref().unwrap().columns, ["pedido_id", "linea"]);
        let fk = items.foreign_keys.iter().find(|f| f.ref_table == "clientes").unwrap();
        assert_eq!(fk.on_delete.as_deref(), Some("SET NULL"));
        let pedidos = &s[2];
        assert!(!pedidos.columns[0].auto_increment, "rowid alias without AUTOINCREMENT");
        assert_eq!(pedidos.foreign_keys[0].ref_columns, ["id"], "implicit key reference");
        let ix = pedidos.indexes.iter().find(|i| i.name == "ix_estado").unwrap();
        assert_eq!(ix.filter.as_deref(), Some("estado IS NOT NULL"));

        // Round trip into a fresh database.
        let mut script = Vec::new();
        for t in &s {
            script.push(table_ddl(t, DdlParts { create: true, if_exists: true, ..Default::default() }));
        }
        for t in &s {
            script.push(table_ddl(t, DdlParts { indexes: true, foreign_keys: true, if_exists: true, ..Default::default() }));
        }
        let script = script.join("\n");
        assert!(script.contains("\"id\" INTEGER PRIMARY KEY AUTOINCREMENT,"), "{script}");
        assert!(script.contains("\n) WITHOUT ROWID;"));
        assert!(script.contains("CREATE UNIQUE INDEX IF NOT EXISTS \"ux_email\" ON \"clientes\" ((lower(email)));"));
        assert!(script.contains("FOREIGN KEY (\"cliente_id\") REFERENCES \"clientes\" (\"id\") ON DELETE SET NULL"));
        let copy = Connection::open_in_memory().unwrap();
        copy.execute_batch(&script).unwrap();
        // The UNIQUE constraint comes back as a unique index of the same name.
        assert_eq!(read_schema(&copy).unwrap(), s);
        // Guards: the same script runs twice.
        copy.execute_batch(&script).unwrap();
    }

    #[test]
    fn checks_are_parsed_from_the_create_statement() {
        let c = parse_checks(
            "CREATE TABLE \"t (x)\" (
                a INTEGER CHECK (a > 0) NOT NULL, -- CHECK (not this)
                \"check\" TEXT CONSTRAINT ck_b CHECK(length(\"check\") < 10),
                c TEXT DEFAULT 'CHECK (x)',
                CONSTRAINT ck_t CHECK (a < 100 AND c IN ('x', 'y')),
                CHECK (a <> 5)
            )",
        );
        let got: Vec<(Option<&str>, &str)> = c.iter().map(|c| (c.name.as_deref(), c.expression.as_str())).collect();
        assert_eq!(
            got,
            [(None, "a > 0"), (Some("ck_b"), "length(\"check\") < 10"), (Some("ck_t"), "a < 100 AND c IN ('x', 'y')"), (None, "a <> 5")]
        );
        assert!(parse_checks("CREATE TABLE t (a)").is_empty());
    }

    #[test]
    fn index_keys_drop_their_order_and_collation() {
        assert_eq!(key_expression("lower(b) DESC"), "lower(b)");
        assert_eq!(key_expression("b COLLATE NOCASE ASC"), "b");
        assert_eq!(key_expression("(a COLLATE x)"), "(a COLLATE x)");
        assert_eq!(key_expression("a + 1"), "a + 1");
    }

    #[test]
    fn checks_order_collation_and_virtual_tables_round_trip() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE p (id INTEGER PRIMARY KEY, n TEXT CHECK (n <> ''), q INT, CONSTRAINT ck_q CHECK (q >= 0));
             CREATE INDEX ix_p ON p (n COLLATE NOCASE DESC, q);
             CREATE INDEX ix_e ON p (lower(n) DESC) WHERE q > 1;
             CREATE VIRTUAL TABLE docs USING fts5(titulo, cuerpo, tokenize = 'porter');
             CREATE VIRTUAL TABLE docs4 USING fts4(a);",
        )
        .unwrap();
        let s = read_schema(&c).unwrap();
        assert_eq!(s.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["p"], "virtual and shadow tables are left out");
        let p = &s[0];
        assert_eq!(p.checks, [CheckDef { name: None, expression: "n <> ''".into() }, CheckDef { name: Some("ck_q".into()), expression: "q >= 0".into() }]);
        let ix = |n: &str| p.indexes.iter().find(|i| i.name == n).unwrap().clone();
        assert_eq!(ix("ix_p").options, [("collate:n".to_string(), "NOCASE".to_string()), ("desc".to_string(), "n".to_string())].into());
        assert_eq!(ix("ix_e").columns, ["(lower(n))"]);
        assert_eq!(ix("ix_e").options.get("desc").map(String::as_str), Some("(lower(n))"));
        let script = [table_ddl(p, DdlParts { create: true, ..Default::default() }), table_ddl(p, DdlParts { indexes: true, ..Default::default() })].join("\n");
        assert!(script.contains("CHECK (n <> '')") && script.contains("CONSTRAINT \"ck_q\" CHECK (q >= 0)"), "{script}");
        assert!(script.contains("(\"n\" COLLATE NOCASE DESC, \"q\")") && script.contains("((lower(n)) DESC) WHERE q > 1"), "{script}");
        let copy = Connection::open_in_memory().unwrap();
        copy.execute_batch(&script).unwrap();
        assert_eq!(read_schema(&copy).unwrap(), s);
    }

    #[test]
    fn a_check_change_rebuilds_the_table() {
        use dbine_driver::TableChange;
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT CONSTRAINT ck_v CHECK (v > 0)); INSERT INTO t VALUES (1, 5);").unwrap();
        let old = read_schema(&c).unwrap().remove(0);
        let mut new = old.clone();
        new.checks = vec![CheckDef { name: Some("ck_v".into()), expression: "v > 1".into() }, CheckDef { name: None, expression: "id < 100".into() }];
        let s = sync_script(&[TableChange::Alter { old: old.clone(), new: new.clone() }]).unwrap();
        assert_eq!(s.statements.len(), 1, "{:?}", s.statements);
        c.execute_batch(&s.statements[0]).unwrap();
        assert_eq!(read_schema(&c).unwrap().remove(0), new);
        // Same CHECKs: nothing to do.
        assert!(sync_script(&[TableChange::Alter { old: new.clone(), new }]).unwrap().statements.is_empty());
    }

    #[test]
    fn index_sql_parts() {
        let (cols, w) = parse_index_sql("CREATE INDEX i ON t (a, lower(b), (c + 1)) WHERE a > 0 AND b IN (1, 2)");
        assert_eq!(cols, ["a", "lower(b)", "(c + 1)"]);
        assert_eq!(w.as_deref(), Some("a > 0 AND b IN (1, 2)"));
        assert_eq!(parse_index_sql("CREATE INDEX i ON t(a)").1, None);
    }

    #[test]
    fn options_follow_the_table() {
        let t = TableSchema {
            name: "t".into(),
            columns: vec![ColumnDef { name: "k".into(), data_type: "TEXT".into(), nullable: false, ..Default::default() }],
            primary_key: Some(KeyDef { name: None, columns: vec!["k".into()] }),
            options: [("without_rowid".to_string(), "true".to_string()), ("strict".to_string(), "true".to_string())].into(),
            ..Default::default()
        };
        let s = table_ddl(&t, DdlParts { create: true, ..Default::default() });
        assert!(s.ends_with("\n) WITHOUT ROWID, STRICT;"), "{s}");
        Connection::open_in_memory().unwrap().execute_batch(&s).unwrap();
    }
}
