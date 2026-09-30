//! Table structure for DuckDB: the catalog read (`database_schema`), the
//! table designer and DDL generation. DuckDB has no identity columns, so
//! auto-increment is a sequence plus `DEFAULT nextval(…)`, and it has no
//! `ALTER TABLE … ADD FOREIGN KEY`, so foreign keys go inside CREATE TABLE.

use dbine_driver::ddl::{self, AutoIncrement, SqlFlavor};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{
    kinds, CheckDef, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, ForeignKeyDef, IndexDef, KeyDef, TableSchema,
};
use std::collections::BTreeMap;

pub const FLAVOR: SqlFlavor = SqlFlavor {
    auto_increment: AutoIncrement::None,
    fk_inline: true,
    ..SqlFlavor::ansi()
};

/// Separator of list items in catalog queries (`array_to_string(…, chr(31))`).
pub const SEP: char = '\u{1f}';

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        schemas: true,
        comments: true,
        ..DesignerSpec::sql_table(vec![
            "INTEGER", "BIGINT", "SMALLINT", "TINYINT", "HUGEINT", "UBIGINT", "DOUBLE", "FLOAT", "DECIMAL(18,2)",
            "BOOLEAN", "VARCHAR", "DATE", "TIME", "TIMESTAMP", "TIMESTAMPTZ", "INTERVAL", "UUID", "BLOB", "JSON",
            "INTEGER[]", "VARCHAR[]", "STRUCT(a INTEGER, b VARCHAR)", "MAP(VARCHAR, INTEGER)",
        ])
    }
}

pub fn templates() -> Vec<CreateTemplate> {
    vec![
        CreateTemplate {
            kind: kinds::VIEW,
            label: "Nueva vista",
            template: "CREATE VIEW {schema}.{name} AS\nSELECT *\nFROM tabla;\n".into(),
        },
        CreateTemplate {
            kind: kinds::FUNCTION,
            label: "Nueva macro",
            template: "CREATE MACRO {schema}.{name}(a, b) AS a + b;\n".into(),
        },
        CreateTemplate {
            kind: kinds::FUNCTION,
            label: "Nueva macro de tabla",
            template: "CREATE MACRO {schema}.{name}(n) AS TABLE\nSELECT * FROM range(n) t(i);\n".into(),
        },
        CreateTemplate {
            kind: kinds::SEQUENCE,
            label: "Nueva secuencia",
            template: "CREATE SEQUENCE {schema}.{name} START 1 INCREMENT 1;\n".into(),
        },
    ]
}

/// Name of the sequence a `nextval('…')` default uses.
fn sequence_of(default: &str) -> Option<&str> {
    let rest = default.trim().strip_prefix("nextval(")?.trim_start();
    let rest = rest.strip_prefix('\'')?;
    Some(&rest[..rest.find('\'')?])
}

/// CREATE TABLE and friends. Auto-increment columns without a default get a
/// sequence `<table>_<column>_seq`; a `nextval` default gets its sequence
/// created if missing, so the script runs on an empty database.
pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let schema = t.schema.as_deref().filter(|s| !s.is_empty());
    let mut t = t.clone();
    let mut sequences = Vec::new();
    for c in &mut t.columns {
        if c.auto_increment && c.default_value.as_deref().is_none_or(|d| d.trim().is_empty()) {
            let seq = format!("{}_{}_seq", t.name, c.name);
            let full = qualified_name(Quote::Double, schema, &seq);
            c.default_value = Some(format!("nextval('{}')", full.replace('\'', "''")));
        }
        if let Some(seq) = c.default_value.as_deref().and_then(sequence_of) {
            sequences.push(seq.replace("''", "'"));
        }
    }
    let mut out = Vec::new();
    if parts.drop {
        out.push(ddl::table_ddl(&FLAVOR, &t, DdlParts { drop: true, if_exists: parts.if_exists, ..Default::default() }));
    }
    if parts.create {
        for s in &sequences {
            // Already qualified and quoted the way nextval() names it.
            out.push(format!("CREATE SEQUENCE IF NOT EXISTS {s};"));
        }
    }
    let rest = DdlParts { drop: false, indexes: false, ..parts };
    let body = ddl::table_ddl(&FLAVOR, &t, rest);
    if !body.is_empty() {
        out.push(body);
    }
    if parts.indexes {
        let name = qualified_name(Quote::Double, schema, &t.name);
        for ix in &t.indexes {
            out.push(format!(
                "CREATE {}INDEX {}{} ON {name} ({});",
                if ix.unique { "UNIQUE " } else { "" },
                if parts.if_exists { "IF NOT EXISTS " } else { "" },
                dbine_driver::sql::quote_ident(Quote::Double, &ix.name),
                ix.columns.iter().map(|c| index_key(c)).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    out.join("\n")
}

/// A sequence's whole `CREATE SEQUENCE`, schema-qualified.
pub fn sequence_sql(schema: &str, name: &str, start: &str, increment: &str, min: &str, max: &str, cycle: bool) -> String {
    format!(
        "CREATE SEQUENCE {} INCREMENT BY {increment} MINVALUE {min} MAXVALUE {max} START WITH {start} {}CYCLE;",
        qualified_name(Quote::Double, Some(schema), name),
        if cycle { "" } else { "NO " }
    )
}

/// An index key: a column name, or an expression kept in parentheses.
fn index_key(c: &str) -> String {
    if c.starts_with('(') && c.ends_with(')') {
        c.to_string()
    } else {
        dbine_driver::sql::quote_ident(Quote::Double, c)
    }
}

// ------------------------------------------------------------ schema sync

fn lower(v: &[String]) -> Vec<String> {
    v.iter().map(|c| c.to_lowercase()).collect()
}

fn fk_key(f: &ForeignKeyDef) -> (Vec<String>, String, Vec<String>) {
    (lower(&f.columns), f.ref_table.to_lowercase(), lower(&f.ref_columns))
}

fn ix_key(i: &IndexDef) -> (Vec<String>, bool) {
    (lower(&i.columns), i.unique)
}

/// What an ALTER of `old` into `new` needs in DuckDB.
enum Plan {
    /// ALTER statements, dropping and recreating the indexes first when a
    /// column change would hit "entries that depend on it".
    Alter { rebuild_indexes: bool },
    /// New table, copy, drop, rename.
    Rebuild,
}

/// DuckDB can't ALTER a table's keys (no DROP CONSTRAINT, no ADD FOREIGN
/// KEY), can't change a column in a key or a unique constraint, and refuses
/// dropping or retyping columns, and nullability changes, while the table
/// has indexes.
fn plan(old: &TableSchema, new: &TableSchema) -> Plan {
    let find = |t: &TableSchema, n: &str| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(n)).cloned();
    let pk = |t: &TableSchema| t.primary_key.as_ref().map(|k| lower(&k.columns)).unwrap_or_default();
    let squash = |s: &str| s.to_lowercase().split_whitespace().collect::<String>();
    let retyped: Vec<String> = new
        .columns
        .iter()
        .filter_map(|n| find(old, &n.name).filter(|o| squash(&o.data_type) != squash(&n.data_type) || o.nullable != n.nullable).map(|_| n.name.to_lowercase()))
        .collect();
    let dropped = old.columns.iter().any(|o| find(new, &o.name).is_none());
    let not_null_added = new.columns.iter().any(|n| !n.nullable && find(old, &n.name).is_none());
    let mut fks_old: Vec<_> = old.foreign_keys.iter().map(fk_key).collect();
    let mut fks_new: Vec<_> = new.foreign_keys.iter().map(fk_key).collect();
    fks_old.sort();
    fks_new.sort();
    let unique_gone = old.indexes.iter().filter(|o| o.unique).any(|o| !new.indexes.iter().any(|n| n.name.eq_ignore_ascii_case(&o.name) && ix_key(n) == ix_key(o)));
    let in_unique = |c: &String| pk(old).contains(c) || old.indexes.iter().any(|i| i.unique && lower(&i.columns).contains(c));
    let touchy = dropped || not_null_added || !retyped.is_empty();
    // No ALTER TABLE … ADD / DROP CONSTRAINT for CHECKs either.
    let checks = |t: &TableSchema| {
        let mut v: Vec<String> = t.checks.iter().map(|c| dbine_driver::alter::check_expr(&c.expression)).collect();
        v.sort();
        v
    };
    if checks(old) != checks(new) || pk(old) != pk(new) || fks_old != fks_new || unique_gone || retyped.iter().any(in_unique) || (touchy && old.indexes.iter().any(|i| i.unique)) {
        Plan::Rebuild
    } else {
        Plan::Alter { rebuild_indexes: touchy && !old.indexes.is_empty() }
    }
}

/// `ALTER COLUMN c TYPE`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT`; a NOT
/// NULL column is added and then made NOT NULL (ADD COLUMN takes no
/// constraints). Key changes rebuild the table.
pub fn sync_script(changes: &[dbine_driver::TableChange]) -> dbine_driver::Result<dbine_driver::SyncScript> {
    use dbine_driver::alter::{self, AlterStyle, ColumnAlter, SyncScript, TableChange};
    const FORCE: &str = "\u{0}rebuild";
    let mut simple: Vec<TableChange> = Vec::new();
    let mut rebuilt: Vec<TableChange> = Vec::new();
    let mut creates: Vec<TableChange> = Vec::new();
    let mut not_null: Vec<(String, String)> = Vec::new();
    for ch in changes {
        match ch {
            TableChange::Create { .. } => creates.push(ch.clone()),
            TableChange::Drop { .. } => simple.push(ch.clone()),
            TableChange::Alter { old, new } => match plan(old, new) {
                Plan::Rebuild => rebuilt.push(ch.clone()),
                Plan::Alter { rebuild_indexes } => {
                    let mut old = old.clone();
                    if rebuild_indexes {
                        // A filter that can't match: the planner drops each index and makes it again.
                        for ix in &mut old.indexes {
                            ix.filter = Some(FORCE.into());
                        }
                    }
                    let name = qualified_name(Quote::Double, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
                    for c in new.columns.iter().filter(|c| !c.nullable && !old.columns.iter().any(|o| o.name.eq_ignore_ascii_case(&c.name))) {
                        let col = dbine_driver::sql::quote_ident(Quote::Double, &c.name);
                        not_null.push((format!("ALTER TABLE {name} ADD COLUMN {col} "), format!("ALTER TABLE {name} ALTER COLUMN {col} SET NOT NULL;")));
                    }
                    simple.push(TableChange::Alter { old, new: new.clone() });
                }
            },
        }
    }
    // ADD COLUMN takes a DEFAULT but no NOT NULL.
    let cd = |t: &TableSchema, c: &ColumnDef| {
        let d = ddl::column_def(&FLAVOR, t, c);
        d.strip_suffix(" NOT NULL").map(str::to_string).unwrap_or(d)
    };
    let dd = |t: &TableSchema, p: DdlParts| Ok(table_ddl(t, p));
    let standard = AlterStyle::from_flavor(&FLAVOR, ColumnAlter::Standard { set_data_type: false, using_cast: false }, &cd, &dd);
    let mut out = SyncScript::default();
    let add = |s: SyncScript, out: &mut SyncScript| {
        for stmt in s.statements {
            let set = not_null.iter().find(|(add, _)| stmt.starts_with(add.as_str())).map(|(_, set)| set.clone());
            out.statements.push(stmt);
            out.statements.extend(set);
        }
        out.warnings.extend(s.warnings);
    };
    if !simple.is_empty() {
        add(alter::sync_script(&standard, &simple)?, &mut out);
    }
    for ch in &rebuilt {
        if let TableChange::Alter { old, new } = ch {
            add(rebuild(old, new), &mut out);
        }
    }
    if !creates.is_empty() {
        add(alter::sync_script(&standard, &creates)?, &mut out);
    }
    Ok(out)
}

/// The table made again: the data aside, the old table dropped, the new one
/// created under its name and filled. Renaming the new table into place (as
/// SQLite does) would leave DuckDB's foreign key bookkeeping pointing at the
/// temporary name, and the referenced tables could no longer be dropped.
fn rebuild(old: &TableSchema, new: &TableSchema) -> dbine_driver::SyncScript {
    let schema = new.schema.as_deref().filter(|s| !s.is_empty());
    let display = match schema {
        Some(s) => format!("{s}.{}", new.name),
        None => new.name.clone(),
    };
    let name = qualified_name(Quote::Double, schema, &old.name);
    let aside = qualified_name(Quote::Double, schema, &format!("{}__dbine_old", old.name));
    let q = |c: &str| dbine_driver::sql::quote_ident(Quote::Double, c);
    let common: Vec<String> = new.columns.iter().filter(|n| old.columns.iter().any(|o| o.name.eq_ignore_ascii_case(&n.name))).map(|c| q(&c.name)).collect();
    let mut warnings: Vec<String> = old
        .columns
        .iter()
        .filter(|o| !new.columns.iter().any(|n| n.name.eq_ignore_ascii_case(&o.name)))
        .map(|c| format!("Se borra la columna {display}.{} con sus datos.", c.name))
        .collect();
    warnings.push(format!(
        "{display} se reconstruye (copia de los datos, tabla nueva y vuelta de los datos): DuckDB no cambia claves ni restricciones con ALTER. Falla si otras tablas la referencian."
    ));
    let mut steps = vec![format!("CREATE TABLE {aside} AS SELECT * FROM {name};"), format!("DROP TABLE {name};"), table_ddl(new, DdlParts { create: true, ..Default::default() })];
    if !common.is_empty() {
        let cols = common.join(", ");
        steps.push(format!("INSERT INTO {} ({cols}) SELECT {cols} FROM {aside};", qualified_name(Quote::Double, schema, &new.name)));
    }
    steps.push(format!("DROP TABLE {aside};"));
    if !new.indexes.is_empty() {
        steps.push(table_ddl(new, DdlParts { indexes: true, ..Default::default() }));
    }
    dbine_driver::SyncScript { statements: vec![steps.join("\n")], warnings }
}

/// Raw catalog rows, as the session's queries return them.
pub struct Catalog {
    /// schema, table, comment
    pub tables: Vec<Vec<Option<String>>>,
    /// schema, table, column, type, nullable, default, comment
    pub columns: Vec<Vec<Option<String>>>,
    /// schema, table, type, name, columns, referenced table, referenced
    /// columns, CHECK expression
    pub constraints: Vec<Vec<Option<String>>>,
    /// schema, table, index, unique, expressions, CREATE INDEX statement
    pub indexes: Vec<Vec<Option<String>>>,
}

fn s(r: &[Option<String>], i: usize) -> String {
    r.get(i).cloned().flatten().unwrap_or_default()
}

fn list(v: &str) -> Vec<String> {
    v.split(SEP).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

/// Index expressions as DuckDB lists them (`[a, "b c"]` or one per item):
/// plain column names, unquoted.
fn index_columns(raw: &str) -> Vec<String> {
    let inner = raw.trim().trim_start_matches('[').trim_end_matches(']');
    list(inner)
        .into_iter()
        .flat_map(|e| e.split(", ").map(str::to_string).collect::<Vec<_>>())
        .map(|e| {
            let e = e.trim();
            match e.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
                Some(q) => q.replace("\"\"", "\""),
                None => e.to_string(),
            }
        })
        .filter(|e| !e.is_empty())
        .collect()
}

/// Split on the commas outside parentheses and quotes.
fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth, mut quote) = (Vec::new(), String::new(), 0i32, None::<char>);
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(std::mem::take(&mut cur).trim().to_string());
                    continue;
                }
                _ => {}
            },
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// `(…)` around the whole text (not `(a) + (b)`).
fn wrapped(s: &str) -> bool {
    if !s.starts_with('(') || !s.ends_with(')') {
        return false;
    }
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != s.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

/// The keys of `CREATE INDEX … ON t(a, (lower(b)))`: column names unquoted,
/// expressions in one pair of parentheses (DuckDB adds its own on each
/// round trip).
pub fn index_sql_keys(sql: &str) -> Vec<String> {
    let upper = sql.to_ascii_uppercase();
    let Some(on) = upper.find(" ON ") else { return Vec::new() };
    let Some(open) = sql[on..].find('(').map(|i| on + i) else { return Vec::new() };
    let Some(close) = sql.rfind(')').filter(|c| *c > open) else { return Vec::new() };
    split_top(&sql[open + 1..close])
        .into_iter()
        .map(|k| {
            let mut e = k.trim().to_string();
            if let Some(n) = e.strip_prefix('"').and_then(|x| x.strip_suffix('"')).filter(|n| !n.replace("\"\"", "").contains('"')) {
                return n.replace("\"\"", "\"");
            }
            if !wrapped(&e) {
                // A bare name.
                if e.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    return e;
                }
            }
            while wrapped(&e) {
                e = e[1..e.len() - 1].trim().to_string();
            }
            format!("({e})")
        })
        .collect()
}

pub fn assemble(cat: Catalog) -> Vec<TableSchema> {
    let key = |r: &[Option<String>]| (s(r, 0), s(r, 1));
    let mut tables: BTreeMap<(String, String), TableSchema> = BTreeMap::new();
    for r in &cat.tables {
        tables.insert(
            key(r),
            TableSchema {
                kind: kinds::TABLE.into(),
                schema: Some(s(r, 0)),
                name: s(r, 1),
                comment: r.get(2).cloned().flatten().filter(|c| !c.is_empty()),
                ..Default::default()
            },
        );
    }
    for r in &cat.columns {
        let Some(t) = tables.get_mut(&key(r)) else { continue };
        let default_value = r.get(5).cloned().flatten();
        t.columns.push(ColumnDef {
            name: s(r, 2),
            data_type: s(r, 3),
            nullable: s(r, 4) == "true",
            auto_increment: default_value.as_deref().is_some_and(|d| sequence_of(d).is_some()),
            default_value,
            comment: r.get(6).cloned().flatten().filter(|c| !c.is_empty()),
            ..Default::default()
        });
    }
    for r in &cat.constraints {
        let Some(t) = tables.get_mut(&key(r)) else { continue };
        let cols = list(&s(r, 4));
        match s(r, 2).as_str() {
            "PRIMARY KEY" => t.primary_key = Some(KeyDef { name: None, columns: cols }),
            // Backed by an ART index, like CREATE UNIQUE INDEX.
            "UNIQUE" => t.indexes.push(IndexDef { name: s(r, 3), columns: cols, unique: true, kind: Some("ART".into()), ..Default::default() }),
            "FOREIGN KEY" => t.foreign_keys.push(ForeignKeyDef {
                name: None,
                columns: cols,
                ref_schema: Some(s(r, 0)),
                ref_table: s(r, 5),
                ref_columns: list(&s(r, 6)),
                ..Default::default()
            }),
            // DuckDB keeps no CHECK names: the ones it reports are made up.
            "CHECK" => t.checks.push(CheckDef { name: None, expression: s(r, 7) }),
            _ => {}
        }
    }
    for r in &cat.indexes {
        let Some(t) = tables.get_mut(&key(r)) else { continue };
        let from_sql = r.get(5).cloned().flatten().map(|sql| index_sql_keys(&sql)).filter(|k| !k.is_empty());
        t.indexes.push(IndexDef {
            name: s(r, 2),
            columns: from_sql.unwrap_or_else(|| index_columns(&s(r, 4))),
            unique: s(r, 3) == "true",
            kind: Some("ART".into()),
            ..Default::default()
        });
    }
    in_dependency_order(tables.into_values().collect())
}

/// Referenced tables before the ones that point at them (foreign keys are
/// inline, so a script must create them in this order); ties by name.
fn in_dependency_order(mut rest: Vec<TableSchema>) -> Vec<TableSchema> {
    let mut out: Vec<TableSchema> = Vec::with_capacity(rest.len());
    while !rest.is_empty() {
        let ready = rest.iter().position(|t| {
            t.foreign_keys.iter().all(|fk| {
                let ref_schema = fk.ref_schema.as_ref().or(t.schema.as_ref());
                let same = |x: &TableSchema| x.name == fk.ref_table && x.schema.as_ref() == ref_schema;
                same(t) || out.iter().any(same) || !rest.iter().any(same)
            })
        });
        // A reference cycle can't be created by DuckDB anyway; keep going.
        out.push(rest.remove(ready.unwrap_or(0)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> TableSchema {
        TableSchema {
            schema: Some("main".into()),
            name: "pedidos".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "INTEGER".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef { name: "cliente_id".into(), data_type: "INTEGER".into(), comment: Some("dueño".into()), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            foreign_keys: vec![ForeignKeyDef {
                columns: vec!["cliente_id".into()],
                ref_table: "clientes".into(),
                ref_columns: vec!["id".into()],
                ..Default::default()
            }],
            indexes: vec![IndexDef { name: "ix_c".into(), columns: vec!["cliente_id".into()], ..Default::default() }],
            comment: Some("Pedidos".into()),
            ..Default::default()
        }
    }

    #[test]
    fn auto_increment_is_a_sequence_and_fks_are_inline() {
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        let s = table_ddl(&t(), all);
        assert!(s.starts_with("DROP TABLE IF EXISTS \"main\".\"pedidos\";\nCREATE SEQUENCE IF NOT EXISTS \"main\".\"pedidos_id_seq\";"), "{s}");
        assert!(s.contains("\"id\" INTEGER DEFAULT nextval('\"main\".\"pedidos_id_seq\"') NOT NULL"), "{s}");
        assert!(s.contains("FOREIGN KEY (\"cliente_id\") REFERENCES \"main\".\"clientes\" (\"id\")\n);"), "{s}");
        assert!(s.contains("COMMENT ON COLUMN \"main\".\"pedidos\".\"cliente_id\" IS 'dueño';"));
        assert!(s.contains("CREATE INDEX IF NOT EXISTS \"ix_c\" ON \"main\".\"pedidos\" (\"cliente_id\");"));
        assert!(!s.contains("ALTER TABLE"));
    }

    #[test]
    fn existing_nextval_default_recreates_its_sequence() {
        let mut tt = t();
        tt.columns[0].default_value = Some("nextval('seq_x')".into());
        let s = table_ddl(&tt, DdlParts { create: true, ..Default::default() });
        assert!(s.starts_with("CREATE SEQUENCE IF NOT EXISTS seq_x;"), "{s}");
        assert!(s.contains("DEFAULT nextval('seq_x')"));
    }

    #[test]
    fn index_expressions_parse() {
        assert_eq!(index_columns("[a, \"b c\"]"), vec!["a", "b c"]);
        assert_eq!(index_columns(&format!("a{SEP}b")), vec!["a", "b"]);
    }

    #[test]
    fn index_keys_from_the_statement() {
        assert_eq!(index_sql_keys("CREATE INDEX ix_e ON t((lower(a)), id);"), ["(lower(a))", "id"]);
        assert_eq!(index_sql_keys("CREATE INDEX ix_p ON s.t(((id + 1)), \"b c\", \"x\"\"y\");"), ["(id + 1)", "b c", "x\"y"]);
        assert_eq!(index_sql_keys("CREATE INDEX i ON t((a || ','), f(b, c));"), ["(a || ',')", "(f(b, c))"]);
        let t = TableSchema {
            schema: Some("main".into()),
            name: "t".into(),
            indexes: vec![IndexDef { name: "i".into(), columns: vec!["(lower(a))".into(), "b".into()], ..Default::default() }],
            ..Default::default()
        };
        assert_eq!(table_ddl(&t, DdlParts { indexes: true, ..Default::default() }), "CREATE INDEX \"i\" ON \"main\".\"t\" ((lower(a)), \"b\");");
    }

    #[test]
    fn sequences_are_whole() {
        assert_eq!(
            sequence_sql("s2", "sq", "7", "1", "1", "9223372036854775807", false),
            "CREATE SEQUENCE \"s2\".\"sq\" INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 7 NO CYCLE;"
        );
    }

    #[test]
    fn a_check_change_rebuilds() {
        use dbine_driver::alter::TableChange;
        let mut old = simple();
        old.indexes.clear();
        old.checks = vec![CheckDef { name: None, expression: "(length(n) > 1)".into() }];
        let mut new = old.clone();
        new.checks[0].expression = "length(n) > 2".into();
        let s = sync_script(&[TableChange::Alter { old: old.clone(), new: new.clone() }]).unwrap();
        assert!(s.statements[0].starts_with("CREATE TABLE \"main\".\"t__dbine_old\""), "{:?}", s.statements);
        assert!(s.statements[0].contains("CHECK (length(n) > 2)"), "{:?}", s.statements);
        // The same condition, spelled as DuckDB reports it: nothing to do.
        new.checks[0].expression = "length(n) > 1".into();
        assert!(sync_script(&[TableChange::Alter { old, new }]).unwrap().statements.is_empty());
    }

    #[test]
    fn referenced_tables_come_first() {
        let child = t();
        let parent = TableSchema { schema: Some("main".into()), name: "clientes".into(), ..Default::default() };
        let order = in_dependency_order(vec![child, parent]);
        assert_eq!(order[0].name, "clientes");
    }
    fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }
    }

    fn simple() -> TableSchema {
        TableSchema {
            schema: Some("main".into()),
            name: "t".into(),
            columns: vec![col("id", "INTEGER", false), col("n", "VARCHAR", false), col("d", "DATE", true)],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            indexes: vec![IndexDef { name: "ix_n".into(), columns: vec!["n".into()], ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn sync_alters_and_moves_indexes_out_of_the_way() {
        use dbine_driver::alter::TableChange;
        let old = simple();
        let mut new = simple();
        new.columns[1].data_type = "VARCHAR(40)".into();
        new.columns[1].nullable = true;
        new.columns[2].default_value = Some("current_date".into());
        new.columns.push(ColumnDef { default_value: Some("'x'".into()), ..col("e", "VARCHAR", false) });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            [
                "DROP INDEX \"main\".\"ix_n\";",
                "ALTER TABLE \"main\".\"t\" ADD COLUMN \"e\" VARCHAR DEFAULT 'x';",
                "ALTER TABLE \"main\".\"t\" ALTER COLUMN \"e\" SET NOT NULL;",
                "ALTER TABLE \"main\".\"t\" ALTER COLUMN \"n\" TYPE VARCHAR(40);",
                "ALTER TABLE \"main\".\"t\" ALTER COLUMN \"n\" DROP NOT NULL;",
                "ALTER TABLE \"main\".\"t\" ALTER COLUMN \"d\" SET DEFAULT current_date;",
                "CREATE INDEX \"ix_n\" ON \"main\".\"t\" (\"n\");",
            ]
        );
        // Only a default: the index stays.
        let mut new = simple();
        new.columns[1].default_value = Some("'z'".into());
        let s = sync_script(&[TableChange::Alter { old: simple(), new }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE \"main\".\"t\" ALTER COLUMN \"n\" SET DEFAULT 'z';"]);
    }

    #[test]
    fn sync_rebuilds_for_keys() {
        use dbine_driver::alter::TableChange;
        let mut new = simple();
        new.foreign_keys.push(ForeignKeyDef { columns: vec!["id".into()], ref_table: "p".into(), ref_columns: vec!["id".into()], ..Default::default() });
        let created = TableSchema { name: "nueva".into(), ..simple() };
        let s = sync_script(&[TableChange::Create { table: created }, TableChange::Alter { old: simple(), new }]).unwrap();
        assert_eq!(
            s.statements[0],
            "CREATE TABLE \"main\".\"t__dbine_old\" AS SELECT * FROM \"main\".\"t\";\n\
DROP TABLE \"main\".\"t\";\n\
CREATE TABLE \"main\".\"t\" (\n    \"id\" INTEGER NOT NULL,\n    \"n\" VARCHAR NOT NULL,\n    \"d\" DATE NULL,\n    PRIMARY KEY (\"id\"),\n    \
FOREIGN KEY (\"id\") REFERENCES \"main\".\"p\" (\"id\")\n);\n\
INSERT INTO \"main\".\"t\" (\"id\", \"n\", \"d\") SELECT \"id\", \"n\", \"d\" FROM \"main\".\"t__dbine_old\";\n\
DROP TABLE \"main\".\"t__dbine_old\";\n\
CREATE INDEX \"ix_n\" ON \"main\".\"t\" (\"n\");"
        );
        assert!(s.statements[1].starts_with("CREATE TABLE \"main\".\"nueva\""));
        // Retyping a key column rebuilds too.
        let mut new = simple();
        new.columns[0].data_type = "BIGINT".into();
        let s = sync_script(&[TableChange::Alter { old: simple(), new }]).unwrap();
        assert!(s.statements[0].starts_with("CREATE TABLE \"main\".\"t__dbine_old\""), "{:?}", s.statements);
    }
}
