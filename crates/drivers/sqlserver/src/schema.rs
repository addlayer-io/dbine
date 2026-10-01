//! Database structure (tables, keys, indexes, extended-property comments),
//! the table designer and the DDL / INSERT scripts in T-SQL.

use dbine_driver::ddl::{self, AutoIncrement, SqlFlavor};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, ForeignKeyDef, IndexDef, KeyDef, RowChange, TableSchema,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub const FLAVOR: SqlFlavor = SqlFlavor {
    quote: Quote::Bracket,
    auto_increment: AutoIncrement::Identity,
    comment_on: false,
    inline_comments: false,
    // DROP TABLE IF EXISTS exists (2016+), CREATE … IF NOT EXISTS doesn't:
    // the guards are written here.
    if_exists: false,
    fk_inline: false,
    multi_row_insert: true,
    true_literal: "1",
    false_literal: "0",
};

/// Rows per `INSERT … VALUES`: the server takes at most 1000.
pub const INSERT_BATCH: usize = 1000;

pub fn designer() -> DesignerSpec {
    DesignerSpec {
        schemas: true,
        comments: true,
        ..DesignerSpec::sql_table(vec![
            "int",
            "bigint",
            "smallint",
            "tinyint",
            "bit",
            "decimal(18,2)",
            "numeric(18,0)",
            "money",
            "float",
            "real",
            "nvarchar(50)",
            "nvarchar(255)",
            "nvarchar(max)",
            "varchar(50)",
            "varchar(max)",
            "nchar(10)",
            "char(10)",
            "date",
            "time(7)",
            "datetime2(7)",
            "datetimeoffset(7)",
            "datetime",
            "smalldatetime",
            "uniqueidentifier",
            "varbinary(max)",
            "varbinary(50)",
            "xml",
            "rowversion",
        ])
    }
}

pub fn create_templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(
            kinds::VIEW,
            "Nueva vista",
            "CREATE OR ALTER VIEW [{schema}].[{name}]\nAS\nSELECT\n    t.id,\n    t.nombre\nFROM [{schema}].[tabla] AS t\nWHERE t.activo = 1;\nGO\n",
        ),
        t(
            kinds::PROCEDURE,
            "Nuevo procedimiento",
            "CREATE OR ALTER PROCEDURE [{schema}].[{name}]\n    @id int,\n    @nombre nvarchar(100) = NULL\nAS\nBEGIN\n    SET NOCOUNT ON;\n\n    SELECT *\n    FROM [{schema}].[tabla]\n    WHERE id = @id;\nEND;\nGO\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función",
            "CREATE OR ALTER FUNCTION [{schema}].[{name}] (@valor int)\nRETURNS int\nAS\nBEGIN\n    RETURN @valor * 2;\nEND;\nGO\n\n-- Función con valores de tabla:\n-- CREATE OR ALTER FUNCTION [{schema}].[{name}] (@id int)\n-- RETURNS TABLE\n-- AS\n-- RETURN (SELECT * FROM [{schema}].[tabla] WHERE id = @id);\n",
        ),
        t(
            kinds::TRIGGER,
            "Nuevo trigger",
            "CREATE OR ALTER TRIGGER [{schema}].[{name}]\nON [{schema}].[tabla]\nAFTER INSERT, UPDATE\nAS\nBEGIN\n    SET NOCOUNT ON;\n\n    UPDATE t\n    SET modificado = SYSDATETIME()\n    FROM [{schema}].[tabla] AS t\n    JOIN inserted AS i ON i.id = t.id;\nEND;\nGO\n",
        ),
    ]
}

fn q(s: &str) -> String {
    quote_ident(Quote::Bracket, s)
}

/// `N'…'`: every string literal is Unicode, so accents survive any collation.
fn nlit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn schema_of(t: &TableSchema) -> &str {
    t.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("dbo")
}

fn comment_stmt(t: &TableSchema, column: Option<&str>, text: &str) -> String {
    let mut s = format!(
        "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = {}, \
         @level0type = N'SCHEMA', @level0name = {}, @level1type = N'TABLE', @level1name = {}",
        nlit(text),
        nlit(schema_of(t)),
        nlit(&t.name)
    );
    if let Some(c) = column {
        s.push_str(&format!(", @level2type = N'COLUMN', @level2name = {}", nlit(c)));
    }
    s.push(';');
    s
}

/// A comment change for the schema sync (`MS_Description`): added or
/// updated (as a clone writes it), or dropped when `text` is `None`.
pub fn comment_change(t: &TableSchema, column: Option<&ColumnDef>, text: Option<&str>) -> String {
    let mut levels = vec![("SCHEMA".to_string(), schema_of(t).to_string()), ("TABLE".to_string(), t.name.clone())];
    if let Some(c) = column {
        levels.push(("COLUMN".to_string(), c.name.clone()));
    }
    match text {
        Some(v) => crate::clone::extended_property(&crate::clone::ExtendedProperty {
            name: "MS_Description".into(),
            value: v.into(),
            base_type: "nvarchar".into(),
            levels,
        }),
        None => {
            let list: Vec<String> = (0..3)
                .map(|i| levels.get(i).map_or("NULL, NULL".to_string(), |(ty, nm)| format!("{}, {}", nlit(ty), nlit(nm))))
                .collect();
            let args: Vec<String> =
                levels.iter().enumerate().map(|(i, (ty, nm))| format!("@level{i}type = {}, @level{i}name = {}", nlit(ty), nlit(nm))).collect();
            format!(
                "IF EXISTS (SELECT 1 FROM sys.fn_listextendedproperty(N'MS_Description', {}))\n    EXEC sys.sp_dropextendedproperty @name = N'MS_Description', {};",
                list.join(", "),
                args.join(", ")
            )
        }
    }
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let name = qualified_name(Quote::Bracket, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
    let object_id = |kind: &str| format!("OBJECT_ID({}, N'{kind}')", nlit(&name));
    let mut out: Vec<String> = Vec::new();

    if parts.drop {
        out.push(if parts.if_exists { format!("DROP TABLE IF EXISTS {name};") } else { format!("DROP TABLE {name};") });
    }

    if parts.create {
        let mut create = ddl::table_ddl(&FLAVOR, t, DdlParts { create: true, ..Default::default() });
        // IDENTITY with its own seed / increment and named DEFAULTs (the
        // generic builder writes `IDENTITY(1,1)` and unnamed defaults).
        for c in &t.columns {
            create = column_extras(create, c);
        }
        // A computed column takes no NULL / NOT NULL (only after PERSISTED).
        for c in t.columns.iter().filter(|c| c.data_type.trim_start().to_ascii_uppercase().starts_with("AS ")) {
            let line = format!("    {} {}", q(&c.name), c.data_type);
            create = create.replace(&format!("{line} NULL"), &line).replace(&format!("{line} NOT NULL"), &line);
        }
        // Only one clustered index per table: the key gives way when another
        // index is the clustered one.
        if t.indexes.iter().any(|i| i.kind.as_deref().is_some_and(|k| k.starts_with("CLUSTERED"))) {
            create = create.replacen(" PRIMARY KEY (", " PRIMARY KEY NONCLUSTERED (", 1);
        }
        let mut stmts = vec![create];
        if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
            stmts.push(comment_stmt(t, None, c));
        }
        for col in &t.columns {
            if let Some(c) = col.comment.as_deref().filter(|c| !c.is_empty()) {
                stmts.push(comment_stmt(t, Some(&col.name), c));
            }
        }
        if parts.if_exists && !parts.drop {
            let body = stmts.join("\n").lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n");
            out.push(format!("IF {} IS NULL\nBEGIN\n{body}\nEND;", object_id("U")));
        } else {
            out.extend(stmts);
        }
    }

    if parts.indexes {
        // Every kind (INCLUDE, WITH options, columnstore, XML, spatial,
        // full-text), in an order the server accepts.
        out.extend(crate::structure::index_statements(&name, t, parts.if_exists));
    }

    if parts.foreign_keys {
        for fk in &t.foreign_keys {
            // Rebuild through the generic builder: one FK, one ALTER.
            let one = TableSchema { schema: t.schema.clone(), name: t.name.clone(), foreign_keys: vec![fk.clone()], ..Default::default() };
            let mut s = ddl::table_ddl(&FLAVOR, &one, DdlParts { foreign_keys: true, ..Default::default() });
            if parts.if_exists {
                if let Some(n) = fk.name.as_deref().filter(|n| !n.is_empty()) {
                    let fk_name = qualified_name(Quote::Bracket, Some(schema_of(t)), n);
                    s = format!("IF OBJECT_ID({}, N'F') IS NULL\n    {s}", nlit(&fk_name));
                }
            }
            out.push(s);
        }
    }
    out.join("\n")
}

/// [`ColumnDef::options`] key: an IDENTITY's `seed,increment` when it isn't
/// `1,1` (read by `database_schema`).
pub const IDENTITY_OPTION: &str = "identity";
/// [`ColumnDef::options`] key: the name of the column's DEFAULT constraint,
/// written as `CONSTRAINT [name] DEFAULT …` (unnamed: the server makes one up).
pub const DEFAULT_NAME_OPTION: &str = "default_constraint";

/// `seed,increment` as T-SQL takes it (numbers only), `None` if it isn't.
fn identity_args(v: &str) -> Option<String> {
    let (seed, inc) = v.split_once(',')?;
    let num = |s: &str| {
        let s = s.trim();
        let digits = s.strip_prefix('-').unwrap_or(s);
        (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')).then(|| s.to_string())
    };
    Some(format!("{},{}", num(seed)?, num(inc)?))
}

/// The column's line in `create` with its IDENTITY seed / increment and
/// its DEFAULT's name, from [`IDENTITY_OPTION`] / [`DEFAULT_NAME_OPTION`].
fn column_extras(create: String, c: &ColumnDef) -> String {
    let identity = c.options.get(IDENTITY_OPTION).filter(|_| c.auto_increment).and_then(|v| identity_args(v));
    let default = c.default_value.as_deref().filter(|d| !d.is_empty());
    let named = c.options.get(DEFAULT_NAME_OPTION).filter(|n| !n.is_empty() && default.is_some());
    if identity.is_none() && named.is_none() {
        return create;
    }
    // Every column name is bracketed: one line starts with it.
    let head = format!("    {} {}", q(&c.name), c.data_type);
    create
        .split('\n')
        .map(|l| {
            let Some(rest) = l.strip_prefix(&head) else { return l.to_string() };
            let mut rest = rest.to_string();
            if let Some(i) = &identity {
                rest = rest.replacen(" IDENTITY(1,1)", &format!(" IDENTITY({i})"), 1);
            }
            if let (Some(n), Some(d)) = (named, default) {
                rest = rest.replacen(&format!(" DEFAULT {d}"), &format!(" CONSTRAINT {} DEFAULT {d}", q(n)), 1);
            }
            format!("{head}{rest}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every IDENTITY column: `(schema, table, column, seed, increment)`.
pub const IDENTITY_SQL: &str = "
SELECT s.name, t.name, ic.name, CAST(ic.seed_value AS nvarchar(40)), CAST(ic.increment_value AS nvarchar(40))
  FROM sys.identity_columns ic
  JOIN sys.tables t ON t.object_id = ic.object_id
  JOIN sys.schemas s ON s.schema_id = t.schema_id
 WHERE t.is_ms_shipped = 0";

// --- Fabric Data Warehouse -----------------------------------------------

/// Fabric's warehouse types: no Unicode (n…) types, no datetime / money /
/// xml, datetime2 and time up to 6 digits.
pub fn fabric_designer() -> DesignerSpec {
    DesignerSpec {
        schemas: true,
        comments: false,
        indexes: false,
        ..DesignerSpec::sql_table(vec![
            "int",
            "bigint",
            "smallint",
            "bit",
            "decimal(18,2)",
            "numeric(18,0)",
            "float",
            "real",
            "varchar(50)",
            "varchar(255)",
            "varchar(8000)",
            "varchar(max)",
            "char(10)",
            "date",
            "time(6)",
            "datetime2(6)",
            "uniqueidentifier",
            "varbinary(8000)",
            "varbinary(max)",
        ])
    }
}

/// T-SQL for a Fabric warehouse table: keys go as `NOT ENFORCED`
/// constraints added with ALTER TABLE (the only way Fabric takes them),
/// IDENTITY has no seed nor increment, and there are no indexes nor
/// extended properties.
pub fn fabric_table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let name = qualified_name(Quote::Bracket, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
    let mut out: Vec<String> = Vec::new();
    if parts.drop {
        out.push(if parts.if_exists { format!("DROP TABLE IF EXISTS {name};") } else { format!("DROP TABLE {name};") });
    }
    if parts.create {
        let bare = TableSchema { primary_key: None, foreign_keys: Vec::new(), indexes: Vec::new(), comment: None, ..t.clone() };
        let create = ddl::table_ddl(&FLAVOR, &bare, DdlParts { create: true, ..Default::default() }).replace(" IDENTITY(1,1)", " IDENTITY");
        let mut stmts = vec![create];
        if let Some(pk) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
            let cname = pk.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("PK_{}", t.name));
            let cols: Vec<String> = pk.columns.iter().map(|c| q(c)).collect();
            stmts.push(format!(
                "ALTER TABLE {name} ADD CONSTRAINT {} PRIMARY KEY NONCLUSTERED ({}) NOT ENFORCED;",
                q(&cname),
                cols.join(", ")
            ));
        }
        if parts.if_exists && !parts.drop {
            let object_id = format!("OBJECT_ID({}, N'U')", nlit(&name));
            let body = stmts.join("\n").lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n");
            out.push(format!("IF {object_id} IS NULL\nBEGIN\n{body}\nEND;"));
        } else {
            out.extend(stmts);
        }
    }
    if parts.indexes {
        for ix in &t.indexes {
            let cols: Vec<String> = ix.columns.iter().map(|c| q(c)).collect();
            if ix.unique {
                out.push(format!(
                    "ALTER TABLE {name} ADD CONSTRAINT {} UNIQUE NONCLUSTERED ({}) NOT ENFORCED;",
                    q(&ix.name),
                    cols.join(", ")
                ));
            } else {
                out.push(format!("-- Índice {} omitido: los almacenes de Fabric no tienen índices.", ix.name));
            }
        }
    }
    if parts.foreign_keys {
        for (i, fk) in t.foreign_keys.iter().enumerate() {
            let cname = fk.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("FK_{}_{}", t.name, i + 1));
            let rs = fk.ref_schema.as_deref().filter(|s| !s.is_empty()).or(t.schema.as_deref().filter(|s| !s.is_empty()));
            let cols: Vec<String> = fk.columns.iter().map(|c| q(c)).collect();
            let refs: Vec<String> = fk.ref_columns.iter().map(|c| q(c)).collect();
            out.push(format!(
                "ALTER TABLE {name} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) NOT ENFORCED;",
                q(&cname),
                cols.join(", "),
                qualified_name(Quote::Bracket, rs, &fk.ref_table),
                refs.join(", ")
            ));
        }
    }
    out.join("\n")
}

/// A value as a T-SQL literal: `N'…'` strings, `1`/`0` booleans, `0x…` binaries.
pub fn literal(v: &Value) -> String {
    match v {
        Value::String(s) if is_hex_binary(s) => s.clone(),
        Value::String(s) => nlit(s),
        Value::Array(_) | Value::Object(_) => nlit(&v.to_string()),
        other => ddl::sql_literal(&FLAVOR, other),
    }
}

/// `0x…` as cells carry binaries (`json_bytes`).
fn is_hex_binary(s: &str) -> bool {
    s.len() > 2 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn insert_script(schema: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    let name = qualified_name(Quote::Bracket, schema.filter(|s| !s.is_empty()), table);
    let cols: Vec<String> = columns.iter().map(|c| q(c)).collect();
    rows.chunks(INSERT_BATCH)
        .map(|chunk| {
            let tuples: Vec<String> = chunk
                .iter()
                .map(|r| format!("({})", r.iter().map(literal).collect::<Vec<_>>().join(", ")))
                .collect();
            format!("INSERT INTO {name} ({}) VALUES\n  {};", cols.join(", "), tuples.join(",\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `UPDATE … WHERE <key>` per changed row, with the same T-SQL literals.
pub fn update_script(schema: Option<&str>, table: &str, changes: &[RowChange]) -> String {
    ddl::update_script_with(Quote::Bracket, schema, table, changes, &literal)
}

pub fn delete_script(schema: Option<&str>, table: &str, keys: &[Vec<(String, serde_json::Value)>]) -> String {
    ddl::delete_script_with(Quote::Bracket, schema, table, keys, &literal)
}

// --- Catalog --------------------------------------------------------------

pub const TABLES_SQL: &str = "
SELECT s.name, t.name, CAST(ep.value AS nvarchar(max))
  FROM sys.tables t
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  LEFT JOIN sys.extended_properties ep
    ON ep.class = 1 AND ep.major_id = t.object_id AND ep.minor_id = 0 AND ep.name = N'MS_Description'
 WHERE t.is_ms_shipped = 0
 ORDER BY s.name, t.name";

pub const COLUMNS_SQL: &str = "
SELECT s.name, t.name, c.name, TYPE_NAME(c.user_type_id), CAST(c.max_length AS int),
       CAST(c.precision AS int), CAST(c.scale AS int), c.is_nullable, c.is_identity,
       OBJECT_DEFINITION(c.default_object_id), cc.definition, CAST(cc.is_persisted AS bit),
       CAST(ep.value AS nvarchar(max))
  FROM sys.columns c
  JOIN sys.tables t ON t.object_id = c.object_id
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id
  LEFT JOIN sys.extended_properties ep
    ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = N'MS_Description'
 WHERE t.is_ms_shipped = 0
 ORDER BY s.name, t.name, c.column_id";

/// Primary keys and indexes: key columns in key order, then the included
/// ones (a columnstore index's columns are all "included").
pub const INDEXES_SQL: &str = "
SELECT s.name, t.name, i.name, i.is_primary_key, i.is_unique, i.type_desc, i.filter_definition, c.name,
       ic.is_included_column
  FROM sys.indexes i
  JOIN sys.tables t ON t.object_id = i.object_id
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
  JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
 WHERE t.is_ms_shipped = 0 AND i.index_id > 0 AND i.is_hypothetical = 0
   AND (ic.key_ordinal > 0 OR ic.is_included_column = 1 OR i.type IN (5, 6))
 ORDER BY s.name, t.name, i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id";

pub const FOREIGN_KEYS_SQL: &str = "
SELECT s.name, t.name, fk.name, pc.name, rs.name, rt.name, rc.name,
       fk.delete_referential_action_desc, fk.update_referential_action_desc
  FROM sys.foreign_keys fk
  JOIN sys.tables t ON t.object_id = fk.parent_object_id
  JOIN sys.schemas s ON s.schema_id = t.schema_id
  JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id
  JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
  JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
  JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
  JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
 WHERE t.is_ms_shipped = 0
 ORDER BY s.name, t.name, fk.name, fkc.constraint_column_id";

/// `NO_ACTION` is the default; the others as SQL writes them.
pub fn fk_rule(desc: Option<String>) -> Option<String> {
    desc.filter(|d| d != "NO_ACTION").map(|d| d.replace('_', " "))
}

/// Tables keyed by (schema, name), in catalog order.
#[derive(Default)]
pub struct Builder {
    order: Vec<(String, String)>,
    tables: BTreeMap<(String, String), TableSchema>,
}

impl Builder {
    pub fn table(&mut self, schema: String, name: String, comment: Option<String>) {
        let key = (schema.clone(), name.clone());
        self.order.push(key.clone());
        self.tables.insert(
            key,
            TableSchema { kind: kinds::TABLE.into(), schema: Some(schema), name, comment, ..Default::default() },
        );
    }

    pub fn get(&mut self, schema: &str, name: &str) -> Option<&mut TableSchema> {
        self.tables.get_mut(&(schema.to_string(), name.to_string()))
    }

    pub fn column(&mut self, schema: &str, table: &str, col: ColumnDef) {
        if let Some(t) = self.get(schema, table) {
            t.columns.push(col);
        }
    }

    /// One row of [`INDEXES_SQL`].
    #[allow(clippy::too_many_arguments)]
    pub fn index_column(
        &mut self,
        schema: &str,
        table: &str,
        index: String,
        primary: bool,
        unique: bool,
        kind: String,
        filter: Option<String>,
        column: String,
        included: bool,
    ) {
        let Some(t) = self.get(schema, table) else { return };
        if primary {
            let pk = t.primary_key.get_or_insert_with(|| KeyDef { name: Some(index), columns: Vec::new() });
            pk.columns.push(column);
            return;
        }
        // INCLUDE (…) of a rowstore index; a columnstore index lists them all as its columns.
        let include = included && !kind.to_ascii_uppercase().contains("COLUMNSTORE");
        match t.indexes.last_mut() {
            Some(ix) if ix.name == index && include => ix.include.push(column),
            Some(ix) if ix.name == index => ix.columns.push(column),
            _ => t.indexes.push(IndexDef {
                name: index,
                columns: vec![column],
                unique,
                kind: Some(kind),
                // `([x] IS NOT NULL)` → `[x] IS NOT NULL`
                filter: filter.map(|f| strip_parens(&f).to_string()),
                ..Default::default()
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn fk_column(
        &mut self,
        schema: &str,
        table: &str,
        name: String,
        column: String,
        ref_schema: String,
        ref_table: String,
        ref_column: String,
        on_delete: Option<String>,
        on_update: Option<String>,
    ) {
        let Some(t) = self.get(schema, table) else { return };
        match t.foreign_keys.last_mut() {
            Some(fk) if fk.name.as_deref() == Some(name.as_str()) => {
                fk.columns.push(column);
                fk.ref_columns.push(ref_column);
            }
            _ => t.foreign_keys.push(ForeignKeyDef {
                name: Some(name),
                columns: vec![column],
                ref_schema: Some(ref_schema),
                ref_table,
                ref_columns: vec![ref_column],
                on_delete,
                on_update,
            }),
        }
    }

    /// One row of [`IDENTITY_SQL`]: only a seed / increment other than
    /// `1,1` goes into the column's options (the default stays implicit).
    pub fn identity(&mut self, schema: &str, table: &str, column: &str, seed: &str, increment: &str) {
        let (seed, increment) = (seed.trim(), increment.trim());
        if seed == "1" && increment == "1" {
            return;
        }
        if let Some(c) = self.get(schema, table).and_then(|t| t.columns.iter_mut().find(|c| c.name == column)) {
            c.options.insert(IDENTITY_OPTION.into(), format!("{seed},{increment}"));
        }
    }

    pub fn finish(mut self) -> Vec<TableSchema> {
        self.order.iter().filter_map(|k| self.tables.remove(k)).collect()
    }
}

/// One pair of wrapping parentheses off, when they wrap the whole text.
pub fn strip_parens(s: &str) -> &str {
    let t = s.trim();
    if !(t.starts_with('(') && t.ends_with(')')) {
        return t;
    }
    let mut depth = 0;
    for (i, ch) in t.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != t.len() - 1 {
                    return t;
                }
            }
            _ => {}
        }
    }
    &t[1..t.len() - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_script_uses_unicode_literals() {
        let c = RowChange {
            key: vec![("id".into(), json!(7)), ("region".into(), Value::Null)],
            set: vec![("nombre".into(), json!("O'Brien")), ("baja".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(Some("dbo"), "clientes", &[c]),
            "UPDATE [dbo].[clientes] SET [nombre] = N'O''Brien', [baja] = NULL WHERE [id] = 7 AND [region] IS NULL;"
        );
    }

    #[test]
    fn fabric_keys_are_not_enforced_and_there_are_no_indexes() {
        let t = sample();
        let all = DdlParts { create: true, indexes: true, foreign_keys: true, ..Default::default() };
        let s = fabric_table_ddl(&t, all);
        assert!(s.contains(" IDENTITY NOT NULL") || s.contains(" IDENTITY,") || s.contains(" IDENTITY\n"), "{s}");
        assert!(!s.contains("IDENTITY(1,1)"), "{s}");
        assert!(!s.contains("sp_addextendedproperty"), "{s}");
        assert!(s.contains("PRIMARY KEY NONCLUSTERED ([id]) NOT ENFORCED;"), "{s}");
        assert!(s.lines().filter(|l| l.contains("FOREIGN KEY")).all(|l| l.ends_with("NOT ENFORCED;")), "{s}");
        assert!(!s.contains("CREATE INDEX") && !s.contains("CREATE NONCLUSTERED INDEX"), "{s}");
        let create = s.split(';').next().unwrap();
        assert!(!create.contains("PRIMARY KEY"), "keys go in ALTER TABLE: {s}");
    }

    fn sample() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("ventas".into()),
            name: "pedidos".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "int".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef { name: "cliente_id".into(), data_type: "int".into(), comment: Some("Dueño".into()), ..Default::default() },
                ColumnDef { name: "total".into(), data_type: "decimal(18,2)".into(), nullable: false, default_value: Some("((0))".into()), ..Default::default() },
                ColumnDef { name: "doble".into(), data_type: "AS ([total]*(2))".into(), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: Some("PK_pedidos".into()), columns: vec!["id".into()] }),
            foreign_keys: vec![ForeignKeyDef {
                name: Some("FK_pedidos_clientes".into()),
                columns: vec!["cliente_id".into()],
                ref_schema: Some("ventas".into()),
                ref_table: "clientes".into(),
                ref_columns: vec!["id".into()],
                on_delete: Some("CASCADE".into()),
                on_update: None,
            }],
            indexes: vec![IndexDef {
                name: "IX_total".into(),
                columns: vec!["total".into()],
                unique: true,
                kind: Some("NONCLUSTERED".into()),
                filter: Some("[total] IS NOT NULL".into()),
                ..Default::default()
            }],
            comment: Some("Pedidos del cliente".into()),
            ..Default::default()
        }
    }

    const ALL: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: true };

    #[test]
    fn create_with_identity_comments_indexes_and_fks() {
        let s = table_ddl(&sample(), ALL);
        assert!(s.starts_with("CREATE TABLE [ventas].[pedidos] (\n    [id] int IDENTITY(1,1) NOT NULL,"), "{s}");
        assert!(s.contains("[total] decimal(18,2) DEFAULT ((0)) NOT NULL,"));
        assert!(s.contains("    [doble] AS ([total]*(2)),\n"), "{s}");
        assert!(s.contains("CONSTRAINT [PK_pedidos] PRIMARY KEY ([id])"));
        assert!(s.contains("EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'Pedidos del cliente', @level0type = N'SCHEMA', @level0name = N'ventas', @level1type = N'TABLE', @level1name = N'pedidos';"));
        assert!(s.contains("@level2type = N'COLUMN', @level2name = N'cliente_id';"));
        assert!(s.contains("CREATE UNIQUE NONCLUSTERED INDEX [IX_total] ON [ventas].[pedidos] ([total]) WHERE [total] IS NOT NULL;"));
        assert!(s.contains("ALTER TABLE [ventas].[pedidos] ADD CONSTRAINT [FK_pedidos_clientes] FOREIGN KEY ([cliente_id]) REFERENCES [ventas].[clientes] ([id]) ON DELETE CASCADE;"));
    }

    #[test]
    fn identity_seed_increment_and_named_defaults() {
        let mut t = sample();
        t.columns[0].options.insert(IDENTITY_OPTION.into(), "1000,5".into());
        t.columns[2].options.insert(DEFAULT_NAME_OPTION.into(), "DF_pedidos_total".into());
        let s = table_ddl(&t, ALL);
        assert!(s.contains("    [id] int IDENTITY(1000,5) NOT NULL,"), "{s}");
        assert!(s.contains("    [total] decimal(18,2) CONSTRAINT [DF_pedidos_total] DEFAULT ((0)) NOT NULL,"), "{s}");
        // Anything but numbers is not written.
        t.columns[0].options.insert(IDENTITY_OPTION.into(), "1,1) x(".into());
        assert!(table_ddl(&t, ALL).contains("[id] int IDENTITY(1,1) NOT NULL"));
        assert_eq!(identity_args("-10, 2").as_deref(), Some("-10,2"));
        // Only a non-default seed / increment is kept.
        let mut b = Builder::default();
        b.table("dbo".into(), "t".into(), None);
        b.column("dbo", "t", ColumnDef { name: "id".into(), data_type: "int".into(), auto_increment: true, ..Default::default() });
        b.column("dbo", "t", ColumnDef { name: "id2".into(), data_type: "int".into(), auto_increment: true, ..Default::default() });
        b.identity("dbo", "t", "id", "1", "1");
        b.identity("dbo", "t", "id2", "1000", "5");
        let t = b.finish().remove(0);
        assert!(t.columns[0].options.is_empty());
        assert_eq!(t.columns[1].options.get(IDENTITY_OPTION).map(String::as_str), Some("1000,5"));
    }

    #[test]
    fn guards_when_if_exists() {
        let s = table_ddl(&sample(), DdlParts { if_exists: true, ..ALL });
        assert!(s.starts_with("IF OBJECT_ID(N'[ventas].[pedidos]', N'U') IS NULL\nBEGIN\n    CREATE TABLE"), "{s}");
        assert!(s.contains("IF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE object_id = OBJECT_ID(N'[ventas].[pedidos]') AND name = N'IX_total')\n    CREATE UNIQUE"));
        assert!(s.contains("IF OBJECT_ID(N'[ventas].[FK_pedidos_clientes]', N'F') IS NULL\n    ALTER TABLE"));
        let d = table_ddl(&sample(), DdlParts { drop: true, if_exists: true, create: true, ..Default::default() });
        assert!(d.starts_with("DROP TABLE IF EXISTS [ventas].[pedidos];\nCREATE TABLE"), "{d}");
    }

    #[test]
    fn clustered_index_moves_the_key_to_nonclustered() {
        let mut t = sample();
        t.indexes[0].kind = Some("CLUSTERED".into());
        let s = table_ddl(&t, ALL);
        assert!(s.contains("CONSTRAINT [PK_pedidos] PRIMARY KEY NONCLUSTERED ([id])"), "{s}");
        assert!(s.contains("CREATE UNIQUE CLUSTERED INDEX [IX_total]"));
    }

    #[test]
    fn inserts_use_unicode_literals_bits_and_1000_row_batches() {
        let rows = vec![vec![json!(1), json!("Año O'Brien"), json!(true), json!("0xFF00"), Value::Null]];
        let s = insert_script(Some("dbo"), "t", &["a".into(), "b".into(), "c".into(), "d".into(), "e".into()], &rows);
        assert_eq!(s, "INSERT INTO [dbo].[t] ([a], [b], [c], [d], [e]) VALUES\n  (1, N'Año O''Brien', 1, 0xFF00, NULL);");
        let many: Vec<Vec<Value>> = (0..2500).map(|i| vec![json!(i)]).collect();
        assert_eq!(insert_script(None, "t", &["a".into()], &many).matches("INSERT INTO").count(), 3);
    }

    #[test]
    fn filters_lose_their_outer_parens() {
        assert_eq!(strip_parens("([a] IS NOT NULL)"), "[a] IS NOT NULL");
        assert_eq!(strip_parens("([a]>(1)) AND ([b]<(2))"), "([a]>(1)) AND ([b]<(2))");
        assert_eq!(fk_rule(Some("SET_NULL".into())).as_deref(), Some("SET NULL"));
        assert_eq!(fk_rule(Some("NO_ACTION".into())), None);
    }

    /// The basic reading (Babelfish, or when the detailed one fails) keeps
    /// INCLUDE columns apart from the key, as the detailed one does.
    #[test]
    fn basic_index_rows_keep_included_columns_apart() {
        let mut b = Builder::default();
        b.table("dbo".into(), "d".into(), None);
        let row = |b: &mut Builder, ix: &str, kind: &str, col: &str, inc: bool| b.index_column("dbo", "d", ix.into(), false, false, kind.into(), None, col.into(), inc);
        // In INDEXES_SQL's order: keys first, then the included ones.
        row(&mut b, "IX", "NONCLUSTERED", "EntityChangeId", false);
        for c in ["Id", "Module"] {
            row(&mut b, "IX", "NONCLUSTERED", c, true);
        }
        for c in ["a", "b"] {
            row(&mut b, "NCCI", "NONCLUSTERED COLUMNSTORE", c, true);
        }
        let t = b.finish().remove(0);
        assert_eq!((t.indexes[0].columns.as_slice(), t.indexes[0].include.as_slice()), (&["EntityChangeId".to_string()][..], &["Id".to_string(), "Module".to_string()][..]));
        assert_eq!(t.indexes[1].columns, ["a", "b"]);
        assert!(t.indexes[1].include.is_empty());
    }
}
