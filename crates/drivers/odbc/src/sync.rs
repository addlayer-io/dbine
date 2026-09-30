//! Schema sync per preset: the shared planner with each engine's ALTER
//! syntax (rewritten where the engine differs), and Hive's own statements
//! for Hive, Impala and Spark.

use crate::design::{self, eng, flavor, has_indexes, reports_foreign_keys, Eng};
use crate::presets::Preset;
use dbine_driver::alter::{self, AlterStyle, ColumnAlter, DropIndex, SyncScript, TableChange};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{ddl, ColumnDef, DdlParts, Error, Result, TableSchema};
use std::collections::HashSet;

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn find<'a>(t: &'a TableSchema, name: &str) -> Option<&'a ColumnDef> {
    t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name))
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

fn default_of(c: &ColumnDef) -> Option<String> {
    c.default_value.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

pub fn sync_script(p: &Preset, changes: &[TableChange]) -> Result<SyncScript> {
    match eng(p) {
        Eng::NetSuite => Err(Error::Unsupported("SuiteAnalytics Connect es de solo lectura: no se cambian tablas".into())),
        Eng::Hive | Eng::Impala | Eng::Spark => Ok(hive(p, changes)),
        _ => sql(p, changes),
    }
}

/// Tables as the engine keeps them: no indexes or foreign keys where it has none.
fn prepare(p: &Preset, t: &TableSchema) -> TableSchema {
    let mut t = t.clone();
    if !has_indexes(p) {
        t.indexes.clear();
    }
    if !reports_foreign_keys(p) {
        t.foreign_keys.clear();
    }
    t
}

/// The quoted identifier at the start of `s` and what follows it.
fn split_ident(s: &str, q: Quote) -> Option<(&str, &str)> {
    let (open, close) = match q {
        Quote::Double => ('"', '"'),
        Quote::Bracket => ('[', ']'),
        Quote::Backtick => ('`', '`'),
    };
    let mut chars = s.char_indices();
    if chars.next()?.1 != open {
        return None;
    }
    let bytes: Vec<(usize, char)> = chars.collect();
    let mut i = 0;
    while i < bytes.len() {
        let (pos, ch) = bytes[i];
        if ch == close {
            // A doubled closing quote is an escaped one.
            if bytes.get(i + 1).is_some_and(|(_, c)| *c == close) {
                i += 2;
                continue;
            }
            let end = pos + ch.len_utf8();
            return Some((&s[..end], s[end..].trim_start()));
        }
        i += 1;
    }
    None
}

fn sql(p: &Preset, changes: &[TableChange]) -> Result<SyncScript> {
    let e = eng(p);
    let f = flavor(p);
    let q = f.quote;
    let changes: Vec<TableChange> = changes
        .iter()
        .map(|c| match c {
            TableChange::Create { table } => TableChange::Create { table: prepare(p, table) },
            TableChange::Drop { table } => TableChange::Drop { table: prepare(p, table) },
            TableChange::Alter { old, new } => TableChange::Alter { old: prepare(p, old), new: prepare(p, new) },
        })
        .collect();

    let std = |set_data_type| ColumnAlter::Standard { set_data_type, using_cast: false };
    let column = match e {
        Eng::Db2 | Eng::Db2i | Eng::Db2zos | Eng::Vertica | Eng::MonetDb | Eng::Ignite3 => std(true),
        Eng::Ase | Eng::Informix | Eng::Cubrid | Eng::Virtuoso | Eng::Zen => ColumnAlter::Modify { keyword: "MODIFY" },
        // Teradata changes an existing column with ADD and its new definition.
        Eng::Teradata => ColumnAlter::Modify { keyword: "ADD" },
        Eng::Exasol => ColumnAlter::Modify { keyword: "MODIFY COLUMN" },
        Eng::Ingres | Eng::Access => ColumnAlter::Modify { keyword: "ALTER COLUMN" },
        Eng::Sqla | Eng::Altibase | Eng::Dameng | Eng::Iris | Eng::MaxDb => ColumnAlter::Oracle,
        _ => ColumnAlter::None,
    };
    let modify = matches!(column, ColumnAlter::Modify { .. });

    // Where MODIFY carries no comment, comment changes go apart (COMMENT ON
    // if the engine has it) and don't make a MODIFY of their own.
    let mut comments: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    // Existing columns that change, for engines whose column change takes no DEFAULT.
    let mut changed: HashSet<(String, String)> = HashSet::new();
    // (statement prefix, statement to add after it)
    let mut after: Vec<(String, String)> = Vec::new();
    // (plain DROP INDEX, engine's DROP INDEX)
    let mut index_drops: Vec<(String, String)> = Vec::new();
    let changes: Vec<TableChange> = changes
        .into_iter()
        .map(|c| {
            let TableChange::Alter { old, mut new } = c else { return c };
            let schema = new.schema.clone().filter(|s| !s.is_empty());
            let name = qualified_name(q, schema.as_deref(), &new.name);
            let tname = display(&new);
            for n in &mut new.columns {
                let Some(o) = find(&old, &n.name) else { continue };
                if modify && !f.inline_comments && o.comment.as_deref().unwrap_or("") != n.comment.as_deref().unwrap_or("") {
                    if f.comment_on {
                        comments.push(format!("COMMENT ON COLUMN {name}.{} IS {};", quote_ident(q, &n.name), n.comment.as_deref().map(lit).unwrap_or_else(|| "NULL".into())));
                    }
                    n.comment = o.comment.clone();
                }
                let default_changed = default_of(o) != default_of(n);
                let other = squash(&o.data_type) != squash(&n.data_type) || o.nullable != n.nullable || default_changed;
                if other && matches!(e, Eng::Ase | Eng::Access) {
                    changed.insert((new.name.to_lowercase(), n.name.to_lowercase()));
                    if default_changed {
                        let col = quote_ident(q, &n.name);
                        if e == Eng::Ase {
                            after.push((
                                format!("ALTER TABLE {name} MODIFY {col} "),
                                format!("ALTER TABLE {name} REPLACE {col} DEFAULT {};", default_of(n).unwrap_or_else(|| "NULL".into())),
                            ));
                        } else {
                            warnings.push(format!("{tname}.{}: Access no cambia el valor por defecto con ALTER COLUMN; se deja como está.", n.name));
                        }
                    }
                }
            }
            if e == Eng::Teradata && new.columns.iter().any(|n| find(&old, &n.name).is_some_and(|o| squash(&o.data_type) != squash(&n.data_type) || o.nullable != n.nullable || default_of(o) != default_of(n))) {
                warnings.push(format!("{tname}: Teradata solo acepta algunos cambios de columna (agrandar un VARCHAR, permitir NULL, el valor por defecto…); los demás fallan."));
            }
            for ix in &old.indexes {
                let plain = format!("DROP INDEX {};", qualified_name(q, schema.as_deref(), &ix.name));
                let ix_name = quote_ident(q, &ix.name);
                let own = match e {
                    Eng::Ase | Eng::Zen => format!("DROP INDEX {}.{ix_name};", quote_ident(q, &new.name)),
                    Eng::Sqla => format!("DROP INDEX {name}.{ix_name};"),
                    _ => continue,
                };
                index_drops.push((plain, own));
            }
            TableChange::Alter { old, new }
        })
        .collect();

    let strip_null = !matches!(e, Eng::Generic | Eng::SqlServer | Eng::Ase | Eng::Sqla);
    let cd = |t: &TableSchema, c: &ColumnDef| {
        let mut c = c.clone();
        design::identity_in_type(e, &mut c);
        if changed.contains(&(t.name.to_lowercase(), c.name.to_lowercase())) {
            // Changed apart (ASE: REPLACE … DEFAULT) or not at all (Access).
            c.default_value = None;
        }
        let mut d = ddl::column_def(&f, t, &c);
        if e == Eng::Ase {
            d = d.replace(" IDENTITY NOT NULL", " IDENTITY");
        }
        if strip_null {
            if let Some(b) = d.strip_suffix(" NULL").filter(|b| !b.ends_with(" NOT")) {
                d = b.to_string();
            }
        }
        d
    };
    let dd = |t: &TableSchema, parts: DdlParts| Ok(design::table_ddl(p, t, parts));
    let mut st = AlterStyle::from_flavor(&f, column, &cd, &dd);
    st.add_column = match e {
        Eng::Generic | Eng::SqlServer | Eng::Ase | Eng::Sqla | Eng::Informix | Eng::Teradata | Eng::Dameng | Eng::Virtuoso | Eng::Zen | Eng::MaxDb | Eng::DBase | Eng::Altibase => "ADD",
        _ => "ADD COLUMN",
    };
    st.drop_index = match e {
        Eng::Teradata | Eng::Cubrid | Eng::MaxDb | Eng::Access => DropIndex::OnTable,
        _ => DropIndex::Plain,
    };
    if e == Eng::Cubrid {
        st.drop_fk = "DROP FOREIGN KEY";
    }
    st.drop_pk_keyword = matches!(
        e,
        Eng::Db2 | Eng::Db2i | Eng::Db2zos | Eng::Sqla | Eng::Exasol | Eng::Altibase | Eng::Cubrid | Eng::Dameng | Eng::Zen | Eng::MaxDb
    );
    let script = alter::sync_script(&st, &changes)?;

    // The engine's own spelling of each statement.
    let mut out: Vec<String> = Vec::new();
    let mut zos_nulls = false;
    for s in script.statements {
        if let Some((_, own)) = index_drops.iter().find(|(plain, _)| *plain == s) {
            out.push(own.clone());
            continue;
        }
        let extra = after.iter().find(|(prefix, _)| s.starts_with(prefix.as_str())).map(|(_, x)| x.clone());
        let rewritten = match s.strip_prefix("ALTER TABLE ") {
            Some(rest) => rewrite(e, q, rest).map(|r| format!("ALTER TABLE {r}")),
            None => Some(s.clone()),
        };
        match rewritten {
            Some(r) => out.push(r),
            None => zos_nulls = true,
        }
        out.extend(extra);
    }
    if zos_nulls {
        warnings.push("Db2 for z/OS no cambia la nulabilidad de una columna con ALTER: esos cambios quedan afuera.".into());
    }
    if e == Eng::Db2 {
        out = with_reorgs(out, &changes, q);
    }
    if e == Eng::Db2zos && out.iter().any(|s| s.contains(" ALTER COLUMN ") || s.contains(" DROP COLUMN ")) {
        warnings.push("Db2 for z/OS: los cambios de columnas pueden dejar el tablespace con REORG pendiente; corré el utilitario REORG después.".into());
    }
    if matches!(e, Eng::Generic | Eng::SqlServer) {
        warnings.push("Conexión ODBC genérica: el script usa SQL estándar y puede necesitar ajustes para tu motor; revisalo antes de ejecutarlo.".into());
    }
    out.extend(comments);
    let mut all = script.warnings;
    all.extend(warnings);
    Ok(SyncScript { statements: out, warnings: all })
}

/// `rest` is an ALTER TABLE statement without its first two words. `None`
/// drops the statement (a change the engine can't make).
fn rewrite(e: Eng, q: Quote, rest: &str) -> Option<String> {
    // The table name: up to the first space outside quotes.
    let mut split = None;
    let mut inside = false;
    for (i, ch) in rest.char_indices() {
        match ch {
            '"' | '`' => inside = !inside,
            '[' => inside = true,
            ']' => inside = false,
            ' ' if !inside => {
                split = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(i) = split else { return Some(rest.to_string()) };
    let (name, tail) = (&rest[..i], &rest[i + 1..]);
    let body = tail.strip_suffix(';').unwrap_or(tail);
    let is_ident = |s: &str| s.starts_with(['"', '[', '`']);
    let new_tail: String = match e {
        Eng::Ase | Eng::Sqla | Eng::Informix | Eng::Teradata if body.starts_with("DROP COLUMN ") => format!("DROP {};", &body[12..]),
        Eng::Netezza | Eng::Ingres if body.starts_with("DROP COLUMN ") || (e == Eng::Ingres && body.starts_with("DROP CONSTRAINT ")) => {
            format!("{body} RESTRICT;")
        }
        Eng::MaxDb if body.starts_with("DROP COLUMN ") => format!("DROP ({});", &body[12..]),
        Eng::MaxDb if body.strip_prefix("ADD ").is_some_and(is_ident) => format!("ADD ({});", &body[4..]),
        Eng::Altibase if body.strip_prefix("ADD ").is_some_and(is_ident) => format!("ADD COLUMN ({});", &body[4..]),
        Eng::Informix if body.starts_with("MODIFY ") => format!("MODIFY ({});", &body[7..]),
        // Informix names a CHECK after it.
        Eng::Informix if body.starts_with("ADD CHECK ") => format!("ADD CONSTRAINT {};", &body[4..]),
        Eng::Informix if body.starts_with("ADD CONSTRAINT ") && body[15..].split_once(" CHECK ").is_some_and(|(n, _)| split_ident(n, q).is_some_and(|(_, rest)| rest.is_empty())) => {
            let (n, cond) = body[15..].split_once(" CHECK ")?;
            format!("ADD CONSTRAINT CHECK {cond} CONSTRAINT {n};")
        }
        Eng::Informix if body.starts_with("ADD CONSTRAINT ") && body.contains(" PRIMARY KEY (") => {
            let (n, keys) = body[15..].split_once(" PRIMARY KEY ")?;
            format!("ADD CONSTRAINT PRIMARY KEY {keys} CONSTRAINT {n};")
        }
        Eng::Informix if body.starts_with("ADD PRIMARY KEY ") => format!("ADD CONSTRAINT {body};"),
        Eng::Sqla | Eng::Iris | Eng::Altibase if body.starts_with("MODIFY (") && body.ends_with(')') => {
            let inner = &body[8..body.len() - 1];
            match e {
                Eng::Sqla => format!("ALTER {inner};"),
                Eng::Iris => format!("ALTER COLUMN {inner};"),
                _ => {
                    // Altibase: MODIFY COLUMN for the type, ALTER COLUMN for the rest.
                    let (col, what) = split_ident(inner, q)?;
                    if what == "NULL" || what == "NOT NULL" {
                        format!("ALTER COLUMN ({col} {what});")
                    } else if let Some(d) = what.strip_prefix("DEFAULT ") {
                        if d == "NULL" { format!("ALTER COLUMN ({col} DROP DEFAULT);") } else { format!("ALTER COLUMN ({col} SET DEFAULT {d});") }
                    } else {
                        format!("MODIFY COLUMN ({inner});")
                    }
                }
            }
        }
        Eng::MonetDb if body.starts_with("ALTER COLUMN ") && body.ends_with(" DROP NOT NULL") => {
            format!("{} SET NULL;", body.trim_end_matches(" DROP NOT NULL"))
        }
        Eng::Db2zos if body.starts_with("ALTER COLUMN ") && body.ends_with(" NOT NULL") => return None,
        _ => return Some(rest.to_string()),
    };
    Some(format!("{name} {new_tail}"))
}

/// Db2 LUW leaves a table in REORG pending after dropping a column,
/// changing nullability or some type changes, and refuses a fourth such
/// change (and most other statements) until it's reorganized: a REORG after
/// every third and after the last one.
fn with_reorgs(statements: Vec<String>, changes: &[TableChange], q: Quote) -> Vec<String> {
    let names: Vec<String> = changes
        .iter()
        .filter_map(|c| match c {
            TableChange::Alter { new, .. } => Some(qualified_name(q, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name)),
            _ => None,
        })
        .collect();
    let pending = |s: &str, name: &str| {
        s.strip_prefix(&format!("ALTER TABLE {name} ")).is_some_and(|t| {
            t.starts_with("DROP COLUMN ") || (t.starts_with("ALTER COLUMN ") && (t.contains(" SET DATA TYPE ") || t.ends_with(" NOT NULL;")))
        })
    };
    let mut out = Vec::new();
    for (i, s) in statements.iter().enumerate() {
        out.push(s.clone());
        for name in &names {
            if !pending(s, name) {
                continue;
            }
            let done = statements[..=i].iter().filter(|x| pending(x, name)).count();
            let more = statements[i + 1..].iter().any(|x| pending(x, name));
            if done % 3 == 0 || !more {
                out.push(format!("CALL SYSPROC.ADMIN_CMD({});", lit(&format!("REORG TABLE {name}"))));
            }
        }
    }
    out
}

/// Hive, Impala and Spark: `ADD COLUMNS`, `CHANGE` (Hive, Impala), `DROP
/// COLUMN` (Impala) or `REPLACE COLUMNS` (Hive); no keys, indexes,
/// NOT NULL or defaults (except Kudu's, which aren't changed here).
fn hive(p: &Preset, changes: &[TableChange]) -> SyncScript {
    let e = eng(p);
    let q = Quote::Backtick;
    let col = |c: &ColumnDef| {
        let mut d = format!("{} {}", quote_ident(q, &c.name), c.data_type);
        if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
            d.push_str(&format!(" COMMENT {}", lit(cm)));
        }
        d
    };
    let (mut drops, mut alters, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Drop { table } => {
                warnings.push(format!("Se borra la tabla {} con todos sus datos.", display(table)));
                drops.push(design::table_ddl(p, table, DdlParts { drop: true, ..Default::default() }));
            }
            TableChange::Create { table } => creates.push(design::table_ddl(p, table, DdlParts { create: true, ..Default::default() })),
            TableChange::Alter { old, new } => {
                let name = qualified_name(q, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
                let tname = display(new);
                let dropped: Vec<&ColumnDef> = old.columns.iter().filter(|o| find(new, &o.name).is_none()).collect();
                let added: Vec<&ColumnDef> = new.columns.iter().filter(|n| find(old, &n.name).is_none()).collect();
                let pk = |t: &TableSchema| t.primary_key.as_ref().map(|k| k.columns.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>());
                if pk(old) != pk(new) {
                    warnings.push(format!("{tname}: la clave primaria no se cambia con ALTER; hay que recrear la tabla."));
                }
                let mut changed = Vec::new();
                for n in &new.columns {
                    let Some(o) = find(old, &n.name) else { continue };
                    if o.nullable != n.nullable || default_of(o) != default_of(n) {
                        warnings.push(format!("{tname}.{}: NOT NULL y los valores por defecto no se cambian con ALTER en este motor; se deja como está.", n.name));
                    }
                    let ty = squash(&o.data_type) != squash(&n.data_type);
                    let cm = o.comment.as_deref().unwrap_or("") != n.comment.as_deref().unwrap_or("");
                    if ty || cm {
                        changed.push((o, n, ty));
                    }
                }
                if e == Eng::Hive && !dropped.is_empty() {
                    // Hive can't drop a column: the whole list is replaced (metadata only).
                    warnings.push(format!(
                        "{tname}: Hive no borra columnas; se reemplaza la lista entera con REPLACE COLUMNS, que cambia solo los metadatos (los datos de los archivos no se tocan)."
                    ));
                    let all: Vec<String> = new.columns.iter().map(col).collect();
                    alters.push(format!("ALTER TABLE {name} REPLACE COLUMNS (\n    {}\n);", all.join(",\n    ")));
                    continue;
                }
                for c in &dropped {
                    if e == Eng::Impala {
                        warnings.push(format!("Se borra la columna {tname}.{} con sus datos.", c.name));
                        alters.push(format!("ALTER TABLE {name} DROP COLUMN {};", quote_ident(q, &c.name)));
                    } else {
                        warnings.push(format!("{tname}.{}: Spark no borra columnas de tablas Hive; se deja como está.", c.name));
                    }
                }
                if !added.is_empty() {
                    let defs: Vec<String> = added.iter().map(|c| col(c)).collect();
                    alters.push(format!("ALTER TABLE {name} ADD COLUMNS ({});", defs.join(", ")));
                }
                for (o, n, ty) in changed {
                    if ty {
                        warnings.push(format!("{tname}.{}: {} → {}. Puede fallar si los tipos no son compatibles.", n.name, o.data_type, n.data_type));
                    }
                    match e {
                        Eng::Spark if ty => warnings.push(format!("{tname}.{}: Spark solo cambia el comentario de una columna de tabla Hive; el tipo se deja como está.", n.name)),
                        Eng::Spark => alters.push(format!(
                            "ALTER TABLE {name} ALTER COLUMN {} COMMENT {};",
                            quote_ident(q, &n.name),
                            lit(n.comment.as_deref().unwrap_or(""))
                        )),
                        _ => alters.push(format!("ALTER TABLE {name} CHANGE COLUMN {} {};", quote_ident(q, &n.name), col(n))),
                    }
                }
            }
        }
    }
    let statements = [drops, alters, creates].into_iter().flatten().filter(|s| !s.trim().is_empty()).collect();
    SyncScript { statements, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;
    use dbine_driver::{IndexDef, KeyDef};

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn col(n: &str, t: &str, null: bool) -> ColumnDef {
        ColumnDef { name: n.into(), data_type: t.into(), nullable: null, ..Default::default() }
    }

    fn old() -> TableSchema {
        TableSchema {
            schema: Some("S".into()),
            name: "T".into(),
            columns: vec![col("ID", "INTEGER", false), col("N", "VARCHAR(10)", false), col("D", "DATE", true)],
            primary_key: Some(KeyDef { name: Some("PK_T".into()), columns: vec!["ID".into()] }),
            indexes: vec![IndexDef { name: "IX_N".into(), columns: vec!["N".into()], ..Default::default() }],
            ..Default::default()
        }
    }

    /// N wider and nullable with a default, D dropped, E added, IX_N on E.
    fn new() -> TableSchema {
        let mut t = old();
        t.columns[1] = ColumnDef { default_value: Some("'x'".into()), ..col("N", "VARCHAR(40)", true) };
        t.columns.retain(|c| c.name != "D");
        t.columns.push(col("E", "INTEGER", true));
        t.indexes[0].columns = vec!["E".into()];
        t
    }

    fn run(id: &str) -> SyncScript {
        sync_script(preset(id), &[TableChange::Alter { old: old(), new: new() }]).unwrap()
    }

    #[test]
    fn db2_alters_and_reorganizes() {
        let s = run("db2");
        assert_eq!(
            s.statements,
            [
                "DROP INDEX \"S\".\"IX_N\";",
                "ALTER TABLE \"S\".\"T\" DROP COLUMN \"D\";",
                "ALTER TABLE \"S\".\"T\" ADD COLUMN \"E\" INTEGER;",
                "ALTER TABLE \"S\".\"T\" ALTER COLUMN \"N\" SET DATA TYPE VARCHAR(40);",
                "ALTER TABLE \"S\".\"T\" ALTER COLUMN \"N\" DROP NOT NULL;",
                "CALL SYSPROC.ADMIN_CMD('REORG TABLE \"S\".\"T\"');",
                "ALTER TABLE \"S\".\"T\" ALTER COLUMN \"N\" SET DEFAULT 'x';",
                "CREATE INDEX \"IX_N\" ON \"S\".\"T\" (\"E\");",
            ]
        );
        // Db2 for i needs no REORG; z/OS doesn't change nullability.
        assert!(!run("db2i").statements.iter().any(|s| s.contains("REORG")));
        let z = run("db2zos");
        assert!(!z.statements.iter().any(|s| s.contains("NOT NULL")), "{:?}", z.statements);
        assert!(z.warnings.iter().any(|w| w.contains("nulabilidad")));
    }

    #[test]
    fn sybase_modifies_and_replaces_defaults() {
        let s = run("sybase");
        assert_eq!(
            s.statements,
            [
                "DROP INDEX [T].[IX_N];",
                "ALTER TABLE [S].[T] DROP [D];",
                "ALTER TABLE [S].[T] ADD [E] INTEGER NULL;",
                "ALTER TABLE [S].[T] MODIFY [N] VARCHAR(40) NULL;",
                "ALTER TABLE [S].[T] REPLACE [N] DEFAULT 'x';",
                "CREATE INDEX [IX_N] ON [S].[T] ([E]);",
            ]
        );
    }

    #[test]
    fn oracle_like_engines_rewrite_modify() {
        let s = run("sqlanywhere");
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ALTER \"N\" VARCHAR(40);".to_string()), "{:?}", s.statements);
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ALTER \"N\" NULL;".to_string()));
        assert!(s.statements.contains(&"DROP INDEX \"S\".\"T\".\"IX_N\";".to_string()));
        let s = run("altibase");
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" MODIFY COLUMN (\"N\" VARCHAR(40));".to_string()), "{:?}", s.statements);
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ALTER COLUMN (\"N\" NULL);".to_string()));
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ALTER COLUMN (\"N\" SET DEFAULT 'x');".to_string()));
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ADD COLUMN (\"E\" INTEGER);".to_string()));
        let s = run("maxdb");
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" DROP (\"D\");".to_string()), "{:?}", s.statements);
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" MODIFY (\"N\" VARCHAR(40));".to_string()));
        assert!(s.statements.contains(&"DROP INDEX \"IX_N\" ON \"S\".\"T\";".to_string()));
    }

    #[test]
    fn informix_and_teradata() {
        let s = run("informix");
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" MODIFY (\"N\" VARCHAR(40) DEFAULT 'x');".to_string()), "{:?}", s.statements);
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" DROP \"D\";".to_string()));
        let mut n = new();
        n.primary_key.as_mut().unwrap().columns.push("E".into());
        let s = sync_script(preset("informix"), &[TableChange::Alter { old: old(), new: n }]).unwrap();
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ADD CONSTRAINT PRIMARY KEY (\"ID\", \"E\") CONSTRAINT \"PK_T\";".to_string()), "{:?}", s.statements);
        let s = run("teradata");
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ADD \"N\" VARCHAR(40) DEFAULT 'x';".to_string()), "{:?}", s.statements);
        assert!(s.statements.contains(&"DROP INDEX \"IX_N\" ON \"S\".\"T\";".to_string()));
        assert!(s.warnings.iter().any(|w| w.contains("Teradata")));
    }

    #[test]
    fn checks_and_included_columns() {
        use dbine_driver::CheckDef;
        let chk = |n: Option<&str>, e: &str| CheckDef { name: n.map(Into::into), expression: e.into() };
        let mut o = old();
        o.checks = vec![chk(Some("CK_N"), "(N <> '')")];
        let mut n = o.clone();
        n.checks = vec![chk(Some("CK_N"), "(N <> 'x')"), chk(None, "(ID > 0)")];
        n.indexes[0].unique = true;
        n.indexes[0].include = vec!["D".into()];
        let s = sync_script(preset("db2"), &[TableChange::Alter { old: o.clone(), new: n.clone() }]).unwrap();
        for want in [
            "ALTER TABLE \"S\".\"T\" DROP CONSTRAINT \"CK_N\";",
            "DROP INDEX \"S\".\"IX_N\";",
            "CREATE UNIQUE INDEX \"IX_N\" ON \"S\".\"T\" (\"N\") INCLUDE (\"D\");",
            "ALTER TABLE \"S\".\"T\" ADD CONSTRAINT \"CK_N\" CHECK (N <> 'x');",
            "ALTER TABLE \"S\".\"T\" ADD CHECK (ID > 0);",
        ] {
            assert!(s.statements.iter().any(|x| x == want), "{want}\n{:#?}", s.statements);
        }
        let s = sync_script(preset("informix"), &[TableChange::Alter { old: o, new: n.clone() }]).unwrap();
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ADD CONSTRAINT CHECK (N <> 'x') CONSTRAINT \"CK_N\";".to_string()), "{:#?}", s.statements);
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ADD CONSTRAINT CHECK (ID > 0);".to_string()), "{:#?}", s.statements);
        let ddl = design::table_ddl(preset("informix"), &n, DdlParts { create: true, ..Default::default() });
        assert!(ddl.contains("    CHECK (N <> 'x') CONSTRAINT \"CK_N\","), "{ddl}");
    }

    #[test]
    fn engines_without_column_changes_warn() {
        for id in ["odbc", "netezza", "ocient", "ignite", "heavydb"] {
            let s = run(id);
            assert!(!s.statements.iter().any(|x| x.contains("VARCHAR(40)")), "{id}: {:?}", s.statements);
            assert!(s.warnings.iter().any(|w| w.contains("no modifica columnas")), "{id}: {:?}", s.warnings);
        }
        assert!(run("netezza").statements.contains(&"ALTER TABLE \"S\".\"T\" DROP COLUMN \"D\" RESTRICT;".to_string()));
        assert!(run("odbc").warnings.iter().any(|w| w.contains("genérica")));
        assert!(sync_script(preset("netsuite"), &[]).is_err());
    }

    #[test]
    fn modify_engines_move_comments_apart() {
        let mut n = old();
        n.columns[1].comment = Some("nombre".into());
        let s = sync_script(preset("exasol"), &[TableChange::Alter { old: old(), new: n }]).unwrap();
        assert_eq!(s.statements, ["COMMENT ON COLUMN \"S\".\"T\".\"N\" IS 'nombre';"]);
        let s = run("monetdb");
        assert!(s.statements.contains(&"ALTER TABLE \"S\".\"T\" ALTER COLUMN \"N\" SET NULL;".to_string()), "{:?}", s.statements);
        let s = run("cubrid");
        assert!(s.statements.contains(&"DROP INDEX \"IX_N\" ON \"S\".\"T\";".to_string()), "{:?}", s.statements);
    }

    #[test]
    fn hive_family() {
        let mut o = old();
        o.columns[1].comment = Some("n".into());
        let s = sync_script(preset("impala"), &[TableChange::Alter { old: o.clone(), new: new() }]).unwrap();
        assert_eq!(
            s.statements,
            [
                "ALTER TABLE `S`.`T` DROP COLUMN `D`;",
                "ALTER TABLE `S`.`T` ADD COLUMNS (`E` INTEGER);",
                "ALTER TABLE `S`.`T` CHANGE COLUMN `N` `N` VARCHAR(40);",
            ]
        );
        let s = sync_script(preset("hive"), &[TableChange::Alter { old: o.clone(), new: new() }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `S`.`T` REPLACE COLUMNS (\n    `ID` INTEGER,\n    `N` VARCHAR(40),\n    `E` INTEGER\n);"]);
        let s = sync_script(preset("spark"), &[TableChange::Alter { old: o, new: new() }]).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `S`.`T` ADD COLUMNS (`E` INTEGER);"]);
        assert!(s.warnings.iter().any(|w| w.contains("Spark no borra")));
    }
}
