//! Schema sync: the statements that turn tables into other versions of
//! themselves (the "Comparar esquemas" tab carries changes from one database
//! to the other, then asks the target's driver for the script).
//!
//! [`sync_script`] plans it for SQL engines from an [`AlterStyle`]: what each
//! family writes to change a column, drop an index or a foreign key, and
//! whether some changes need the table rebuilt (SQLite). The order avoids
//! dependency errors: foreign keys come off first and go back last, and
//! dropped tables go before created ones.

use crate::ddl::{table_ddl as flavor_table_ddl, SqlFlavor};
use crate::schema::{CheckDef, ColumnDef, DdlParts, ForeignKeyDef, IndexDef, TableSchema};
use crate::sql::{qualified_name, quote_ident, Quote};
use crate::Result;
use serde::{Deserialize, Serialize};

/// One table's change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TableChange {
    Create { table: TableSchema },
    Drop { table: TableSchema },
    /// `old` becomes `new` (same table: columns, keys and indexes pair by name).
    Alter { old: TableSchema, new: TableSchema },
}

/// What a sync runs, in order, and what the user should know first.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncScript {
    /// Each one runs on its own (a chunk may hold a few statements, like the
    /// output of `table_ddl`).
    pub statements: Vec<String>,
    /// Data loss, changes that may fail with rows in the table, things left out.
    pub warnings: Vec<String>,
}

/// How a column's type, nullability and default change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlter {
    /// `ALTER COLUMN c TYPE t` (or `SET DATA TYPE t`), `SET/DROP NOT NULL`,
    /// `SET/DROP DEFAULT` (PostgreSQL, Db2, Snowflake, H2, HANA…).
    Standard { set_data_type: bool, using_cast: bool },
    /// `ALTER COLUMN c t [NOT] NULL`; defaults are named constraints (SQL Server, Sybase).
    SqlServer,
    /// `MODIFY [COLUMN] <whole column>` (MySQL family, ClickHouse).
    Modify { keyword: &'static str },
    /// `MODIFY (c t)`, `MODIFY (c [NOT] NULL)`, `MODIFY (c DEFAULT x)` (Oracle).
    Oracle,
    /// The table is rebuilt: new table, copy, drop, rename (SQLite).
    Recreate,
    /// Columns can only be added or dropped.
    None,
}

/// How indexes are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropIndex {
    /// `DROP INDEX [schema.]ix`.
    Plain,
    /// `DROP INDEX ix ON t` (SQL Server).
    OnTable,
    /// `ALTER TABLE t DROP INDEX ix` (MySQL).
    AlterTable,
}

pub struct AlterStyle<'a> {
    pub quote: Quote,
    pub column: ColumnAlter,
    /// `ADD COLUMN` or `ADD`.
    pub add_column: &'static str,
    pub drop_index: DropIndex,
    /// `DROP CONSTRAINT` or `DROP FOREIGN KEY` (MySQL).
    pub drop_fk: &'static str,
    /// `DROP PRIMARY KEY` where the key has no usable name (MySQL, Oracle).
    pub drop_pk_keyword: bool,
    /// Foreign keys live inside CREATE TABLE: changing them rebuilds the table.
    pub fk_inline: bool,
    /// `COMMENT ON COLUMN` for comment changes.
    pub comment_on: bool,
    /// The column as CREATE TABLE writes it (name, type, identity, default, null).
    pub column_def: &'a dyn Fn(&TableSchema, &ColumnDef) -> String,
    /// The driver's own DDL for the parts of a table.
    pub table_ddl: &'a dyn Fn(&TableSchema, DdlParts) -> Result<String>,
}

impl<'a> AlterStyle<'a> {
    /// A style from a [`SqlFlavor`], with the flavor's CREATE TABLE and
    /// column writing.
    pub fn from_flavor(f: &'a SqlFlavor, column: ColumnAlter, column_def: &'a dyn Fn(&TableSchema, &ColumnDef) -> String, table_ddl: &'a dyn Fn(&TableSchema, DdlParts) -> Result<String>) -> Self {
        AlterStyle {
            quote: f.quote,
            column,
            add_column: "ADD COLUMN",
            drop_index: DropIndex::Plain,
            drop_fk: "DROP CONSTRAINT",
            drop_pk_keyword: false,
            fk_inline: f.fk_inline,
            comment_on: f.comment_on,
            column_def,
            table_ddl,
        }
    }
}

/// `table_ddl` for a flavor, as an [`AlterStyle::table_ddl`].
pub fn flavor_ddl(f: &SqlFlavor) -> impl Fn(&TableSchema, DdlParts) -> Result<String> + '_ {
    move |t, p| Ok(flavor_table_ddl(f, t, p))
}

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: false, foreign_keys: false };
const INDEXES: DdlParts = DdlParts { drop: false, if_exists: false, create: false, indexes: true, foreign_keys: false };
const FKS: DdlParts = DdlParts { drop: false, if_exists: false, create: false, indexes: false, foreign_keys: true };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

#[derive(Default)]
struct Plan {
    drop_fks: Vec<String>,
    drop_tables: Vec<String>,
    pre: Vec<String>,
    columns: Vec<String>,
    post: Vec<String>,
    creates: Vec<String>,
    add_fks: Vec<String>,
    warnings: Vec<String>,
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn fk_same(a: &ForeignKeyDef, b: &ForeignKeyDef) -> bool {
    let cols = |x: &[String]| x.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>();
    let act = |x: &Option<String>| x.as_deref().map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty() && s != "NO ACTION" && s != "RESTRICT");
    cols(&a.columns) == cols(&b.columns)
        && eq_name(&a.ref_table, &b.ref_table)
        && cols(&a.ref_columns) == cols(&b.ref_columns)
        && act(&a.on_delete) == act(&b.on_delete)
        && act(&a.on_update) == act(&b.on_update)
}

/// Each old foreign key's counterpart in `new` (its position), one to one:
/// a table can have two keys that link the same columns the same way, and
/// dropping one of them must not look like keeping it. The same name wins;
/// otherwise any equal one still free (names are usually generated).
fn fk_pairs(old: &[ForeignKeyDef], new: &[ForeignKeyDef]) -> (Vec<Option<usize>>, Vec<bool>) {
    let mut used = vec![false; new.len()];
    let mut pairs: Vec<Option<usize>> = vec![None; old.len()];
    let named = |f: &ForeignKeyDef| f.name.clone().filter(|n| !n.is_empty());
    for (i, o) in old.iter().enumerate() {
        let Some(on) = named(o) else { continue };
        if let Some(j) = (0..new.len()).find(|&j| !used[j] && named(&new[j]).is_some_and(|nn| eq_name(&nn, &on)) && fk_same(o, &new[j])) {
            used[j] = true;
            pairs[i] = Some(j);
        }
    }
    for (i, o) in old.iter().enumerate() {
        if pairs[i].is_some() {
            continue;
        }
        if let Some(j) = (0..new.len()).find(|&j| !used[j] && fk_same(o, &new[j])) {
            used[j] = true;
            pairs[i] = Some(j);
        }
    }
    (pairs, used)
}

fn ix_same(a: &IndexDef, b: &IndexDef) -> bool {
    let cols = |x: &[String]| x.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>();
    let w = |x: &Option<String>| x.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let kind = |x: &Option<String>| x.as_deref().unwrap_or("").trim().to_lowercase();
    // Included columns: their order doesn't matter.
    let inc = |x: &[String]| {
        let mut v = cols(x);
        v.sort();
        v
    };
    cols(&a.columns) == cols(&b.columns)
        && a.unique == b.unique
        && w(&a.filter) == w(&b.filter)
        && kind(&a.kind) == kind(&b.kind)
        && inc(&a.include) == inc(&b.include)
        && a.options == b.options
}

fn is_fulltext(ix: &IndexDef) -> bool {
    ix.kind.as_deref().is_some_and(|k| k.trim().eq_ignore_ascii_case("fulltext"))
}

/// A CHECK's condition as compared: case, spaces, identifier quotes and
/// parentheses around the whole condition or around a number don't count
/// (SQL Server stores `precio > 0` as `([precio]>(0))`).
pub fn check_expr(e: &str) -> String {
    let flat: String = e.to_lowercase().chars().filter(|c| !c.is_whitespace() && !matches!(c, '[' | ']' | '"' | '`')).collect();
    // `(0)`, `(-1.5)` → the number.
    let chars: Vec<char> = flat.chars().collect();
    let mut s = String::with_capacity(flat.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' {
            let mut j = i + 1;
            if j < chars.len() && chars[j] == '-' {
                j += 1;
            }
            let digits = j;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                j += 1;
            }
            if j > digits && j < chars.len() && chars[j] == ')' {
                s.extend(&chars[i + 1..j]);
                i = j + 1;
                continue;
            }
        }
        s.push(chars[i]);
        i += 1;
    }
    while s.starts_with('(') && crate::ddl::balanced_outer(&s) {
        s = s[1..s.len() - 1].to_string();
    }
    s
}

fn check_same(a: &CheckDef, b: &CheckDef) -> bool {
    check_expr(&a.expression) == check_expr(&b.expression)
}

/// The other side's CHECK that is the same constraint: by name when both
/// have one, otherwise by condition.
fn find_check<'c>(list: &'c [CheckDef], c: &CheckDef) -> Option<&'c CheckDef> {
    match c.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => list.iter().find(|o| o.name.as_deref().is_some_and(|m| eq_name(m, n))),
        None => list.iter().find(|o| check_same(o, c)),
    }
}

fn norm_default(d: &Option<String>) -> Option<String> {
    d.as_deref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

/// What changed in a column, as far as ALTER goes.
struct ColumnChanges {
    ty: bool,
    null: bool,
    default: bool,
    auto: bool,
    comment: bool,
}

fn column_changes(o: &ColumnDef, n: &ColumnDef) -> ColumnChanges {
    ColumnChanges {
        ty: squash(&o.data_type) != squash(&n.data_type),
        null: o.nullable != n.nullable,
        default: norm_default(&o.default_value) != norm_default(&n.default_value),
        auto: o.auto_increment != n.auto_increment,
        comment: o.comment.as_deref().unwrap_or("") != n.comment.as_deref().unwrap_or(""),
    }
}

impl ColumnChanges {
    fn any(&self) -> bool {
        self.ty || self.null || self.default || self.auto || self.comment
    }
}

/// Plan the changes with `st`.
pub fn sync_script(st: &AlterStyle, changes: &[TableChange]) -> Result<SyncScript> {
    let mut p = Plan::default();
    for ch in changes {
        match ch {
            TableChange::Create { table } => {
                p.creates.push((st.table_ddl)(table, CREATE)?);
                if !table.indexes.is_empty() {
                    p.creates.push((st.table_ddl)(table, INDEXES)?);
                }
                if !table.foreign_keys.is_empty() && !st.fk_inline {
                    p.add_fks.push((st.table_ddl)(table, FKS)?);
                }
            }
            TableChange::Drop { table } => {
                p.warnings.push(format!("Se borra la tabla {} con todos sus datos.", display(table)));
                p.drop_tables.push((st.table_ddl)(table, DROP)?);
            }
            TableChange::Alter { old, new } => alter_table(st, old, new, &mut p)?,
        }
    }
    let statements = [p.drop_fks, p.drop_tables, p.pre, p.columns, p.post, p.creates, p.add_fks].into_iter().flatten().filter(|s| !s.trim().is_empty()).collect();
    Ok(SyncScript { statements, warnings: p.warnings })
}

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn alter_table(st: &AlterStyle, old: &TableSchema, new: &TableSchema, p: &mut Plan) -> Result<()> {
    let name = qualified_name(st.quote, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
    let q = |c: &str| quote_ident(st.quote, c);
    let tname = display(new);

    let dropped: Vec<&ColumnDef> = old.columns.iter().filter(|c| !new.columns.iter().any(|n| eq_name(&n.name, &c.name))).collect();
    let added: Vec<&ColumnDef> = new.columns.iter().filter(|c| !old.columns.iter().any(|o| eq_name(&o.name, &c.name))).collect();
    let changed: Vec<(&ColumnDef, &ColumnDef, ColumnChanges)> = new
        .columns
        .iter()
        .filter_map(|n| old.columns.iter().find(|o| eq_name(&o.name, &n.name)).map(|o| (o, n, column_changes(o, n))))
        .filter(|(_, _, c)| c.any())
        .collect();
    let pk_cols = |t: &TableSchema| t.primary_key.as_ref().map(|k| k.columns.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>()).unwrap_or_default();
    let retyped: Vec<String> = changed.iter().filter(|(_, _, c)| c.ty || c.null).map(|(_, n, _)| n.name.to_lowercase()).collect();
    // A key whose columns change type is dropped and made again.
    let pk_changed = pk_cols(old) != pk_cols(new) || pk_cols(new).iter().any(|c| retyped.contains(c));
    let (fk_pair, fk_kept) = fk_pairs(&old.foreign_keys, &new.foreign_keys);
    let fks_changed = fk_pair.iter().any(Option::is_none) || fk_kept.iter().any(|k| !k);

    let rebuild = st.column == ColumnAlter::Recreate && (!dropped.is_empty() || !changed.is_empty() || pk_changed || (st.fk_inline && fks_changed) || added.iter().any(|c| !c.nullable && norm_default(&c.default_value).is_none()));
    if rebuild || (st.fk_inline && fks_changed) {
        return recreate(st, old, new, p);
    }

    for c in &dropped {
        p.warnings.push(format!("Se borra la columna {tname}.{} con sus datos.", c.name));
    }

    // Foreign keys: the ones that go or change, and the ones on columns that change type.
    let touches = |fk: &ForeignKeyDef| fk.columns.iter().any(|c| retyped.contains(&c.to_lowercase()) || dropped.iter().any(|d| eq_name(&d.name, c)));
    for (o, kept) in old.foreign_keys.iter().zip(&fk_pair) {
        if kept.is_none() || touches(o) {
            match o.name.as_deref().filter(|n| !n.is_empty()) {
                Some(n) => p.drop_fks.push(format!("ALTER TABLE {name} {} {};", st.drop_fk, q(n))),
                None => p.warnings.push(format!("Una clave foránea de {tname} no tiene nombre: no se puede borrar, hacelo a mano.")),
            }
        }
    }
    for (n, kept) in new.foreign_keys.iter().zip(&fk_kept) {
        if !kept || touches(n) {
            let one = TableSchema { foreign_keys: vec![n.clone()], ..new.clone() };
            p.add_fks.push((st.table_ddl)(&one, FKS)?);
        }
    }

    // Indexes: by name; a changed one, or one on a column that changes type (SQL Server, Db2), is made again.
    let rebuild_ix_on_type = matches!(st.column, ColumnAlter::SqlServer);
    let ix_touches = |ix: &IndexDef| rebuild_ix_on_type && ix.columns.iter().any(|c| retyped.contains(&c.to_lowercase()));
    for o in &old.indexes {
        let same = new.indexes.iter().find(|n| eq_name(&n.name, &o.name));
        let goes = same.is_none_or(|n| !ix_same(o, n)) || ix_touches(o) || o.columns.iter().any(|c| dropped.iter().any(|d| eq_name(&d.name, c)));
        if goes {
            p.pre.push(match st.drop_index {
                // A full-text index has no name of its own in SQL Server.
                DropIndex::OnTable if is_fulltext(o) => format!("DROP FULLTEXT INDEX ON {name};"),
                DropIndex::Plain => format!("DROP INDEX {};", qualified_name(st.quote, new.schema.as_deref().filter(|s| !s.is_empty()), &o.name)),
                DropIndex::OnTable => format!("DROP INDEX {} ON {name};", q(&o.name)),
                DropIndex::AlterTable => format!("ALTER TABLE {name} DROP INDEX {};", q(&o.name)),
            });
        }
    }
    let mut new_ix = Vec::new();
    for n in &new.indexes {
        let same = old.indexes.iter().find(|o| eq_name(&o.name, &n.name));
        if same.is_none_or(|o| !ix_same(o, n)) || ix_touches(n) {
            new_ix.push(n.clone());
        }
    }

    // Primary key.
    if pk_changed {
        if old.primary_key.as_ref().is_some_and(|k| !k.columns.is_empty()) {
            match (old.primary_key.as_ref().and_then(|k| k.name.as_deref()).filter(|n| !n.is_empty()), st.drop_pk_keyword) {
                (_, true) => p.pre.push(format!("ALTER TABLE {name} DROP PRIMARY KEY;")),
                (Some(n), false) => p.pre.push(format!("ALTER TABLE {name} DROP CONSTRAINT {};", q(n))),
                (None, false) => p.warnings.push(format!("La clave primaria de {tname} no tiene nombre: borrala a mano antes de sincronizar.")),
            }
        }
    }

    // Columns.
    for c in &dropped {
        if matches!(st.column, ColumnAlter::SqlServer) && norm_default(&c.default_value).is_some() {
            p.columns.push(sqlserver_drop_default(new, &c.name, st.quote));
        }
        p.columns.push(format!("ALTER TABLE {name} DROP COLUMN {};", q(&c.name)));
    }
    for c in &added {
        if !c.nullable && norm_default(&c.default_value).is_none() && !c.auto_increment {
            p.warnings.push(format!("{tname}.{} es NOT NULL sin valor por defecto: falla si la tabla tiene filas.", c.name));
        }
        p.columns.push(format!("ALTER TABLE {name} {} {};", st.add_column, (st.column_def)(new, c)));
    }
    for (o, n, ch) in &changed {
        if ch.ty {
            p.warnings.push(format!("{tname}.{}: {} → {}. Puede fallar o recortar datos si los valores no entran.", n.name, o.data_type, n.data_type));
        }
        if ch.null && !n.nullable {
            p.warnings.push(format!("{tname}.{} pasa a NOT NULL: falla si hay filas con NULL.", n.name));
        }
        if ch.auto && !matches!(st.column, ColumnAlter::Modify { .. }) {
            p.warnings.push(format!("{tname}.{}: el autoincremento no se cambia con ALTER en este motor; se deja como está.", n.name));
        }
        let col = q(&n.name);
        match st.column {
            ColumnAlter::Standard { set_data_type, using_cast } => {
                if ch.ty {
                    let kw = if set_data_type { "SET DATA TYPE" } else { "TYPE" };
                    let using = if using_cast { format!(" USING {col}::{}", n.data_type) } else { String::new() };
                    p.columns.push(format!("ALTER TABLE {name} ALTER COLUMN {col} {kw} {}{using};", n.data_type));
                }
                if ch.null {
                    p.columns.push(format!("ALTER TABLE {name} ALTER COLUMN {col} {} NOT NULL;", if n.nullable { "DROP" } else { "SET" }));
                }
                if ch.default {
                    p.columns.push(match norm_default(&n.default_value) {
                        Some(d) => format!("ALTER TABLE {name} ALTER COLUMN {col} SET DEFAULT {d};"),
                        None => format!("ALTER TABLE {name} ALTER COLUMN {col} DROP DEFAULT;"),
                    });
                }
            }
            ColumnAlter::SqlServer => {
                let has_default = norm_default(&o.default_value).is_some();
                // A default constraint blocks changing the column: off first, back after.
                if has_default && (ch.ty || ch.default) {
                    p.columns.push(sqlserver_drop_default(new, &n.name, st.quote));
                }
                if ch.ty || ch.null {
                    p.columns.push(format!("ALTER TABLE {name} ALTER COLUMN {col} {} {};", n.data_type, if n.nullable { "NULL" } else { "NOT NULL" }));
                }
                if (ch.default || (has_default && ch.ty)) && norm_default(&n.default_value).is_some() {
                    p.columns.push(format!("ALTER TABLE {name} ADD DEFAULT {} FOR {col};", norm_default(&n.default_value).unwrap_or_default()));
                }
            }
            ColumnAlter::Modify { keyword } => {
                p.columns.push(format!("ALTER TABLE {name} {keyword} {};", (st.column_def)(new, n)));
            }
            ColumnAlter::Oracle => {
                if ch.ty {
                    p.columns.push(format!("ALTER TABLE {name} MODIFY ({col} {});", n.data_type));
                }
                if ch.default {
                    p.columns.push(format!("ALTER TABLE {name} MODIFY ({col} DEFAULT {});", norm_default(&n.default_value).unwrap_or_else(|| "NULL".into())));
                }
                if ch.null {
                    p.columns.push(format!("ALTER TABLE {name} MODIFY ({col} {});", if n.nullable { "NULL" } else { "NOT NULL" }));
                }
            }
            ColumnAlter::Recreate => unreachable!("handled by recreate"),
            ColumnAlter::None => {
                if ch.ty || ch.null || ch.default {
                    p.warnings.push(format!("{tname}.{}: este motor no modifica columnas; se deja como está.", n.name));
                }
            }
        }
        if ch.comment && st.comment_on && !matches!(st.column, ColumnAlter::Modify { .. }) {
            p.columns.push(format!("COMMENT ON COLUMN {name}.{col} IS {};", n.comment.as_deref().map(lit).unwrap_or_else(|| "NULL".into())));
        }
    }

    // Back: primary key, indexes.
    if pk_changed {
        if let Some(k) = new.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
            let cols: Vec<String> = k.columns.iter().map(|c| q(c)).collect();
            p.post.push(match k.name.as_deref().filter(|n| !n.is_empty()) {
                Some(n) => format!("ALTER TABLE {name} ADD CONSTRAINT {} PRIMARY KEY ({});", q(n), cols.join(", ")),
                None => format!("ALTER TABLE {name} ADD PRIMARY KEY ({});", cols.join(", ")),
            });
        }
    }
    if !new_ix.is_empty() {
        let only = TableSchema { indexes: new_ix, ..new.clone() };
        p.post.push((st.table_ddl)(&only, INDEXES)?);
    }
    // CHECK constraints: a changed one is dropped and added again.
    for o in &old.checks {
        let kept = find_check(&new.checks, o).is_some_and(|n| check_same(o, n));
        if kept {
            continue;
        }
        match o.name.as_deref().filter(|n| !n.is_empty()) {
            Some(n) => p.pre.push(format!("ALTER TABLE {name} DROP CONSTRAINT {};", q(n))),
            None => p.warnings.push(format!("Una restricción CHECK de {tname} no tiene nombre: borrala a mano ({}).", o.expression.trim())),
        }
    }
    for n in &new.checks {
        if find_check(&old.checks, n).is_some_and(|o| check_same(o, n)) {
            continue;
        }
        let f = crate::ddl::SqlFlavor { quote: st.quote, ..crate::ddl::SqlFlavor::ansi() };
        p.post.push(format!("ALTER TABLE {name} ADD {};", crate::ddl::check_clause(&f, n)));
        p.warnings.push(format!("La restricción CHECK nueva de {tname} falla si hay filas que no la cumplen."));
    }
    if st.comment_on && old.comment.as_deref().unwrap_or("") != new.comment.as_deref().unwrap_or("") {
        p.post.push(format!("COMMENT ON TABLE {name} IS {};", new.comment.as_deref().map(lit).unwrap_or_else(|| "NULL".into())));
    }
    Ok(())
}

/// Drop a column's default constraint, whatever SQL Server named it.
fn sqlserver_drop_default(t: &TableSchema, column: &str, quote: Quote) -> String {
    let obj = qualified_name(quote, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
    format!(
        "DECLARE @df sysname = (SELECT d.name FROM sys.default_constraints d JOIN sys.columns c ON c.object_id = d.parent_object_id AND c.column_id = d.parent_column_id WHERE d.parent_object_id = OBJECT_ID({}) AND c.name = {});\nIF @df IS NOT NULL BEGIN DECLARE @sql nvarchar(max) = N'ALTER TABLE {} DROP CONSTRAINT ' + QUOTENAME(@df); EXEC sp_executesql @sql; END",
        format!("N{}", lit(&obj)),
        format!("N{}", lit(column)),
        obj.replace('\'', "''"),
    )
}

/// Rebuild the table: create the new version aside, copy the columns both
/// have, drop the old one, rename, then the indexes.
fn recreate(st: &AlterStyle, old: &TableSchema, new: &TableSchema, p: &mut Plan) -> Result<()> {
    let tname = display(new);
    let tmp = TableSchema { name: format!("{}__dbine_new", new.name), indexes: Vec::new(), ..new.clone() };
    let q = |c: &str| quote_ident(st.quote, c);
    let schema = new.schema.as_deref().filter(|s| !s.is_empty());
    let common: Vec<String> = new.columns.iter().filter(|n| old.columns.iter().any(|o| eq_name(&o.name, &n.name))).map(|c| q(&c.name)).collect();
    for c in old.columns.iter().filter(|o| !new.columns.iter().any(|n| eq_name(&n.name, &o.name))) {
        p.warnings.push(format!("Se borra la columna {tname}.{} con sus datos.", c.name));
    }
    p.warnings.push(format!("{tname} se reconstruye (tabla nueva, copia de los datos y cambio de nombre): este motor no modifica columnas en el lugar."));
    let mut steps = vec![(st.table_ddl)(&tmp, CREATE)?];
    if !common.is_empty() {
        steps.push(format!(
            "INSERT INTO {} ({cols}) SELECT {cols} FROM {};",
            qualified_name(st.quote, schema, &tmp.name),
            qualified_name(st.quote, schema, &old.name),
            cols = common.join(", ")
        ));
    }
    steps.push(format!("DROP TABLE {};", qualified_name(st.quote, schema, &old.name)));
    steps.push(format!("ALTER TABLE {} RENAME TO {};", qualified_name(st.quote, schema, &tmp.name), q(&new.name)));
    if !new.indexes.is_empty() {
        steps.push((st.table_ddl)(new, INDEXES)?);
    }
    p.columns.push(steps.join("\n"));
    if !st.fk_inline && !new.foreign_keys.is_empty() {
        p.add_fks.push((st.table_ddl)(new, FKS)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::column_def;
    use crate::schema::KeyDef;

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }
    }

    fn table(cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema { kind: "table".into(), schema: Some("dbo".into()), name: "clientes".into(), columns: cols, primary_key: Some(KeyDef { name: Some("PK_c".into()), columns: vec!["id".into()] }), ..Default::default() }
    }

    fn run(column: ColumnAlter, old: TableSchema, new: TableSchema) -> SyncScript {
        let f = SqlFlavor { fk_inline: column == ColumnAlter::Recreate, ..SqlFlavor::ansi() };
        let cd = |t: &TableSchema, c: &ColumnDef| column_def(&f, t, c);
        let dd = flavor_ddl(&f);
        let mut st = AlterStyle::from_flavor(&f, column, &cd, &dd);
        if column == ColumnAlter::SqlServer {
            st.quote = Quote::Bracket;
            st.add_column = "ADD";
            st.drop_index = DropIndex::OnTable;
        }
        sync_script(&st, &[TableChange::Alter { old, new }]).unwrap()
    }

    #[test]
    fn postgres_style() {
        let old = table(vec![col("id", "integer", false), col("nombre", "varchar(10)", true), col("baja", "date", true)]);
        let mut new = table(vec![col("id", "integer", false), col("nombre", "varchar(5)", false), col("email", "text", true)]);
        new.columns[1].default_value = Some("'x'".into());
        let s = run(ColumnAlter::Standard { set_data_type: false, using_cast: true }, old, new);
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"dbo\".\"clientes\" DROP COLUMN \"baja\";",
                "ALTER TABLE \"dbo\".\"clientes\" ADD COLUMN \"email\" text NULL;",
                "ALTER TABLE \"dbo\".\"clientes\" ALTER COLUMN \"nombre\" TYPE varchar(5) USING \"nombre\"::varchar(5);",
                "ALTER TABLE \"dbo\".\"clientes\" ALTER COLUMN \"nombre\" SET NOT NULL;",
                "ALTER TABLE \"dbo\".\"clientes\" ALTER COLUMN \"nombre\" SET DEFAULT 'x';",
            ]
        );
        assert_eq!(s.warnings.len(), 3, "{:?}", s.warnings);
    }

    #[test]
    fn sqlserver_style_moves_defaults_and_indexes() {
        let mut old = table(vec![col("id", "int", false), col("nombre", "varchar(10)", true)]);
        old.columns[1].default_value = Some("('x')".into());
        old.indexes.push(IndexDef { name: "IX_nombre".into(), columns: vec!["nombre".into()], unique: false, kind: None, filter: None, ..Default::default() });
        let mut new = old.clone();
        new.columns[1].data_type = "varchar(5)".into();
        let s = run(ColumnAlter::SqlServer, old, new);
        let all = s.statements.join("\n");
        assert!(s.statements[0].starts_with("DROP INDEX [IX_nombre] ON [dbo].[clientes]"), "{all}");
        assert!(all.contains("sys.default_constraints"), "{all}");
        assert!(all.contains("ALTER TABLE [dbo].[clientes] ALTER COLUMN [nombre] varchar(5) NULL;"), "{all}");
        assert!(all.contains("ALTER TABLE [dbo].[clientes] ADD DEFAULT ('x') FOR [nombre];"), "{all}");
        assert!(all.contains("CREATE INDEX \"IX_nombre\""), "{all}");
    }

    #[test]
    fn included_columns_and_options_make_an_index_different() {
        let ix = IndexDef { name: "IX_n".into(), columns: vec!["nombre".into()], include: vec!["id".into()], ..Default::default() };
        let mut old = table(vec![col("id", "int", false), col("nombre", "varchar(10)", true)]);
        old.indexes.push(ix.clone());
        let mut new = old.clone();
        new.indexes[0].include = vec![];
        let s = run(ColumnAlter::Standard { set_data_type: false, using_cast: false }, old.clone(), new);
        assert_eq!(s.statements, vec!["DROP INDEX \"dbo\".\"IX_n\";", "CREATE INDEX \"IX_n\" ON \"dbo\".\"clientes\" (\"nombre\");"]);
        let mut new = old.clone();
        new.indexes[0].options.insert("fillfactor".into(), "80".into());
        assert_eq!(run(ColumnAlter::Standard { set_data_type: false, using_cast: false }, old.clone(), new).statements.len(), 2);
        // Same included columns in another order: the same index.
        let mut a = old.clone();
        a.indexes[0].include = vec!["id".into(), "nombre".into()];
        let mut b = old;
        b.indexes[0].include = vec!["nombre".into(), "id".into()];
        assert!(run(ColumnAlter::Standard { set_data_type: false, using_cast: false }, a, b).statements.is_empty());
        let t = TableSchema { indexes: vec![ix], ..table(vec![]) };
        assert!(crate::ddl::table_ddl(&SqlFlavor::ansi(), &t, INDEXES).contains("ON \"dbo\".\"clientes\" (\"nombre\") INCLUDE (\"id\");"));
    }

    #[test]
    fn fulltext_indexes_drop_without_a_name_in_sql_server() {
        let mut old = table(vec![col("id", "int", false), col("texto", "nvarchar(max)", true)]);
        old.indexes.push(IndexDef { name: "fulltext".into(), columns: vec!["texto".into()], kind: Some("FULLTEXT".into()), ..Default::default() });
        let new = TableSchema { indexes: vec![], ..old.clone() };
        let s = run(ColumnAlter::SqlServer, old, new);
        assert_eq!(s.statements, vec!["DROP FULLTEXT INDEX ON [dbo].[clientes];"]);
    }

    #[test]
    fn check_constraints_are_compared_and_synced() {
        let chk = |n: Option<&str>, e: &str| CheckDef { name: n.map(Into::into), expression: e.into() };
        let mut old = table(vec![col("id", "int", false), col("precio", "int", true)]);
        old.checks = vec![chk(Some("CK_precio"), "([precio]>(0))"), chk(Some("CK_viejo"), "id > 0"), chk(None, "precio < 100")];
        let mut new = old.clone();
        // Same condition written differently: unchanged.
        new.checks[0].expression = "( [PRECIO] > (0) )".into();
        new.checks[1].expression = "id > 1".into();
        new.checks.remove(2);
        new.checks.push(chk(Some("CK nuevo"), "precio <> 5"));
        let s = run(ColumnAlter::Standard { set_data_type: false, using_cast: false }, old, new);
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"dbo\".\"clientes\" DROP CONSTRAINT \"CK_viejo\";",
                "ALTER TABLE \"dbo\".\"clientes\" ADD CONSTRAINT \"CK_viejo\" CHECK (id > 1);",
                "ALTER TABLE \"dbo\".\"clientes\" ADD CONSTRAINT \"CK nuevo\" CHECK (precio <> 5);",
            ]
        );
        // The unnamed one can't be dropped by name: it's a warning.
        assert!(s.warnings.iter().any(|w| w.contains("no tiene nombre") && w.contains("precio < 100")), "{:?}", s.warnings);
        let t = TableSchema { checks: vec![chk(Some("CK_p"), "(precio > 0)")], ..table(vec![col("id", "int", false)]) };
        let ddl = crate::ddl::table_ddl(&SqlFlavor::ansi(), &t, CREATE);
        assert!(ddl.contains("    CONSTRAINT \"CK_p\" CHECK (precio > 0)\n);"), "{ddl}");
    }

    #[test]
    fn recreate_style_rebuilds() {
        let old = table(vec![col("id", "integer", false), col("nombre", "text", true)]);
        let new = table(vec![col("id", "integer", false), col("nombre", "text", false)]);
        let s = run(ColumnAlter::Recreate, old, new);
        assert_eq!(s.statements.len(), 1);
        let st = &s.statements[0];
        assert!(st.contains("CREATE TABLE \"dbo\".\"clientes__dbine_new\""), "{st}");
        assert!(st.contains("INSERT INTO \"dbo\".\"clientes__dbine_new\" (\"id\", \"nombre\") SELECT \"id\", \"nombre\" FROM \"dbo\".\"clientes\";"), "{st}");
        assert!(st.contains("ALTER TABLE \"dbo\".\"clientes__dbine_new\" RENAME TO \"clientes\";"), "{st}");
    }

    #[test]
    fn creates_and_drops_in_order() {
        let mut a = table(vec![col("id", "integer", false)]);
        a.name = "nueva".into();
        a.foreign_keys.push(ForeignKeyDef { name: Some("fk".into()), columns: vec!["id".into()], ref_schema: None, ref_table: "clientes".into(), ref_columns: vec!["id".into()], on_delete: None, on_update: None });
        let mut b = table(vec![col("id", "integer", false)]);
        b.name = "vieja".into();
        let f = SqlFlavor::ansi();
        let cd = |t: &TableSchema, c: &ColumnDef| column_def(&f, t, c);
        let dd = flavor_ddl(&f);
        let st = AlterStyle::from_flavor(&f, ColumnAlter::Standard { set_data_type: false, using_cast: false }, &cd, &dd);
        let s = sync_script(&st, &[TableChange::Create { table: a }, TableChange::Drop { table: b }]).unwrap();
        assert!(s.statements[0].starts_with("DROP TABLE \"dbo\".\"vieja\""));
        assert!(s.statements[1].starts_with("CREATE TABLE \"dbo\".\"nueva\""));
        assert!(s.statements[2].starts_with("ALTER TABLE \"dbo\".\"nueva\" ADD CONSTRAINT \"fk\" FOREIGN KEY"));
    }

    #[test]
    fn duplicate_foreign_keys_drop_one_to_one() {
        let fk = |name: &str, col: &str| ForeignKeyDef { name: Some(name.into()), columns: vec![col.into()], ref_schema: Some("dbo".into()), ref_table: "AbpUsers".into(), ref_columns: vec!["Id".into()], on_delete: None, on_update: None };
        let mut old = table(vec![col("id", "int", false), col("CreatedById", "bigint", true), col("EvaluatedById", "bigint", true)]);
        old.foreign_keys = vec![fk("FK_C", "CreatedById"), fk("FK_C_2", "CreatedById"), fk("FK_E", "EvaluatedById"), fk("FK_E_2", "EvaluatedById")];
        // The duplicates go; their twins (same columns, same reference) stay.
        let mut new = old.clone();
        new.foreign_keys = vec![old.foreign_keys[0].clone(), old.foreign_keys[2].clone()];
        let s = run(ColumnAlter::SqlServer, old.clone(), new);
        assert_eq!(s.statements, vec!["ALTER TABLE [dbo].[clientes] DROP CONSTRAINT [FK_C_2];", "ALTER TABLE [dbo].[clientes] DROP CONSTRAINT [FK_E_2];"]);
        // Keeping the suffixed one drops the other, by name.
        let mut new = old.clone();
        new.foreign_keys = vec![old.foreign_keys[1].clone(), old.foreign_keys[2].clone(), old.foreign_keys[3].clone()];
        let s = run(ColumnAlter::SqlServer, old.clone(), new);
        assert_eq!(s.statements, vec!["ALTER TABLE [dbo].[clientes] DROP CONSTRAINT [FK_C];"]);
        // Adding a second key equal to an existing one adds it.
        let mut new = old.clone();
        new.foreign_keys.push(fk("FK_C_3", "CreatedById"));
        let s = run(ColumnAlter::SqlServer, old.clone(), new);
        assert_eq!(s.statements.len(), 1);
        assert!(s.statements[0].contains("FK_C_3"), "{:?}", s.statements);
        // Same keys under other generated names: nothing to do.
        let mut new = old.clone();
        for f in &mut new.foreign_keys {
            f.name = Some(format!("{}_x", f.name.as_deref().unwrap()));
        }
        assert!(run(ColumnAlter::SqlServer, old, new).statements.is_empty());
    }
}
