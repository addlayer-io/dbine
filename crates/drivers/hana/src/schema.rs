//! Tables with their keys and indexes from the SYS catalog views of the
//! session's schema, and DDL for them: the ER diagram, the script generator
//! and the table designer.
//!
//! HANA has no `IF [NOT] EXISTS` for tables or indexes: guarded statements
//! run in a `DO BEGIN … END` block that checks the catalog first.

use crate::{format_type, int, quote, text, HanaSession};
use dbine_driver::ddl::{self, AutoIncrement, SqlFlavor};
use dbine_driver::sql::Quote;
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, ForeignKeyDef, IndexDef, KeyDef,
    Result, TableSchema,
};
use serde_json::Value;
use std::collections::HashMap;

/// Table option: `COLUMN` (default) or `ROW` store.
pub const STORE: &str = "store";

pub fn flavor() -> SqlFlavor {
    SqlFlavor {
        quote: Quote::Double,
        auto_increment: AutoIncrement::GeneratedIdentity,
        comment_on: true,
        if_exists: false,
        multi_row_insert: false,
        ..SqlFlavor::ansi()
    }
}

// ---------------------------------------------------------------- catalog

const TABLES: &str = "SELECT TABLE_NAME, COMMENTS, TABLE_TYPE FROM SYS.TABLES
 WHERE SCHEMA_NAME = ? AND IS_SYSTEM_TABLE = 'FALSE' AND IS_USER_DEFINED_TYPE = 'FALSE'
 ORDER BY TABLE_NAME";

const COLUMNS: &str = "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE_NAME, LENGTH, SCALE, IS_NULLABLE, DEFAULT_VALUE,
       GENERATION_TYPE, COMMENTS
  FROM SYS.TABLE_COLUMNS WHERE SCHEMA_NAME = ?
 ORDER BY TABLE_NAME, POSITION";

const PRIMARY_KEYS: &str = "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME FROM SYS.CONSTRAINTS
 WHERE SCHEMA_NAME = ? AND IS_PRIMARY_KEY = 'TRUE'
 ORDER BY TABLE_NAME, POSITION";

/// Indexes and the unique constraints (which HANA backs with one).
const INDEXES: &str = "SELECT i.TABLE_NAME, i.INDEX_NAME, i.INDEX_TYPE, i.CONSTRAINT, c.COLUMN_NAME
  FROM SYS.INDEXES i
  JOIN SYS.INDEX_COLUMNS c
    ON c.SCHEMA_NAME = i.SCHEMA_NAME AND c.TABLE_NAME = i.TABLE_NAME AND c.INDEX_NAME = i.INDEX_NAME
 WHERE i.SCHEMA_NAME = ? AND (i.CONSTRAINT IS NULL OR i.CONSTRAINT <> 'PRIMARY KEY')
 ORDER BY i.TABLE_NAME, i.INDEX_NAME, c.POSITION";

const FOREIGN_KEYS: &str = "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_SCHEMA_NAME,
       REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME, DELETE_RULE, UPDATE_RULE
  FROM SYS.REFERENTIAL_CONSTRAINTS WHERE SCHEMA_NAME = ?
 ORDER BY TABLE_NAME, CONSTRAINT_NAME, POSITION";

/// Names HANA made up (`_SYS_TREE_CS_#…`, `_SYS_CONSTRAINT_…`).
fn user_name(n: String) -> Option<String> {
    (!n.starts_with("_SYS_")).then_some(n)
}

/// `RESTRICT` / `NO ACTION` are the default.
fn rule(r: Option<String>) -> Option<String> {
    r.filter(|r| !matches!(r.as_str(), "RESTRICT" | "NO ACTION" | ""))
}

/// SYS.TABLE_COLUMNS keeps string and date defaults without their quotes.
fn default_literal(data_type: &str, v: &str) -> String {
    let v = v.trim();
    let upper = v.to_ascii_uppercase();
    let quoted = ["CHAR", "TEXT", "ALPHANUM", "DATE", "TIME"].iter().any(|t| data_type.to_ascii_uppercase().contains(t));
    if !quoted || v.starts_with('\'') || upper == "NULL" || upper.starts_with("CURRENT_") {
        v.to_string()
    } else {
        format!("'{}'", v.replace('\'', "''"))
    }
}

pub async fn database_schema(s: &HanaSession) -> Result<Vec<TableSchema>> {
    let schema = s.schema.as_str();
    let mut tables: Vec<TableSchema> = Vec::new();
    let mut pos: HashMap<String, usize> = HashMap::new();
    for r in s.rows(TABLES, &[schema]).await? {
        let name = r.first().and_then(text).unwrap_or_default();
        let mut t = TableSchema {
            kind: kinds::TABLE.into(),
            name: name.clone(),
            comment: r.get(1).and_then(text).filter(|c| !c.is_empty()),
            ..Default::default()
        };
        if let Some(store) = r.get(2).and_then(text).filter(|s| s == "COLUMN" || s == "ROW") {
            t.options.insert(STORE.into(), store);
        }
        pos.insert(name, tables.len());
        tables.push(t);
    }

    for r in s.rows(COLUMNS, &[schema]).await? {
        let t = |i: usize| r.get(i).and_then(text);
        let Some(&ti) = t(0).and_then(|n| pos.get(&n)) else { continue };
        let data_type = format_type(&t(2).unwrap_or_default(), r.get(3).and_then(int), r.get(4).and_then(int));
        tables[ti].columns.push(ColumnDef {
            name: t(1).unwrap_or_default(),
            nullable: t(5).as_deref() != Some("FALSE"),
            default_value: t(6).map(|d| default_literal(&data_type, &d)),
            auto_increment: t(7).is_some_and(|g| g.contains("IDENTITY")),
            comment: t(8).filter(|c| !c.is_empty()),
            data_type,
            ..Default::default()
        });
    }

    for r in s.rows(PRIMARY_KEYS, &[schema]).await? {
        let t = |i: usize| r.get(i).and_then(text);
        let Some(&ti) = t(0).and_then(|n| pos.get(&n)) else { continue };
        let name = t(1).and_then(user_name);
        tables[ti].primary_key.get_or_insert_with(|| KeyDef { name, columns: vec![] }).columns.extend(t(2));
    }

    for r in s.rows(INDEXES, &[schema]).await? {
        let t = |i: usize| r.get(i).and_then(text);
        let Some(&ti) = t(0).and_then(|n| pos.get(&n)) else { continue };
        let name = t(1).unwrap_or_default();
        let table = &mut tables[ti];
        match table.indexes.last_mut().filter(|i| i.name == name) {
            Some(ix) => ix.columns.extend(t(4)),
            None => table.indexes.push(IndexDef {
                name,
                columns: t(4).into_iter().collect(),
                unique: t(3).is_some_and(|c| c.contains("UNIQUE")),
                kind: t(2).filter(|k| !k.is_empty()),
                filter: None,
                ..Default::default()
            }),
        }
    }
    // Unnamed unique constraints get a name the DDL can use.
    for t in &mut tables {
        let mut n = 0;
        for ix in &mut t.indexes {
            if ix.name.starts_with("_SYS_") {
                n += 1;
                ix.name = format!("UK_{}_{n}", t.name);
            }
        }
    }

    for r in s.rows(FOREIGN_KEYS, &[schema]).await? {
        let t = |i: usize| r.get(i).and_then(text);
        let Some(&ti) = t(0).and_then(|n| pos.get(&n)) else { continue };
        let name = t(1);
        let table = &mut tables[ti];
        match table.foreign_keys.last_mut().filter(|f| f.name == name) {
            Some(fk) => {
                fk.columns.extend(t(2));
                fk.ref_columns.extend(t(5));
            }
            None => table.foreign_keys.push(ForeignKeyDef {
                name,
                columns: t(2).into_iter().collect(),
                ref_schema: t(3).filter(|rs| rs != schema),
                ref_table: t(4).unwrap_or_default(),
                ref_columns: t(5).into_iter().collect(),
                on_delete: rule(t(6)),
                on_update: rule(t(7)),
            }),
        }
    }
    for t in &mut tables {
        for fk in &mut t.foreign_keys {
            fk.name = fk.name.take().and_then(user_name);
        }
    }
    crate::structure::complete(s, &mut tables).await;
    Ok(tables)
}

// -------------------------------------------------------------------- DDL

/// `stmt` run only when `catalog` has (`exists`) or lacks a row named
/// `name` in the table's schema.
fn guarded(stmt: &str, exists: bool, catalog: &str, column: &str, schema: Option<&str>, name: &str) -> String {
    let lit = |s: &str| ddl::sql_literal(&flavor(), &Value::String(s.into()));
    let owner = schema.filter(|s| !s.is_empty()).map_or("CURRENT_SCHEMA".to_string(), lit);
    format!(
        "DO BEGIN\n  DECLARE n INTEGER;\n  SELECT COUNT(*) INTO n FROM {catalog} WHERE SCHEMA_NAME = {owner} AND {column} = {};\n  IF :n {} 0 THEN\n    EXEC {};\n  END IF;\nEND;",
        lit(name),
        if exists { ">" } else { "=" },
        lit(stmt)
    )
}

/// Index types `CREATE … INDEX` takes.
const INDEX_TYPES: &[&str] = &["BTREE", "CPBTREE", "INVERTED VALUE", "INVERTED HASH", "INVERTED INDIVIDUAL"];

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let f = flavor();
    let schema = t.schema.as_deref().filter(|s| !s.is_empty());
    let name = dbine_driver::sql::qualified_name(Quote::Double, schema, &t.name);
    let mut out: Vec<String> = Vec::new();

    if parts.drop {
        let drop = format!("DROP TABLE {name}");
        out.push(if parts.if_exists { guarded(&drop, true, "SYS.TABLES", "TABLE_NAME", schema, &t.name) } else { format!("{drop};") });
    }

    if parts.create {
        let generic = ddl::table_ddl(&f, t, DdlParts { create: true, ..Default::default() });
        let end = generic.find("\n);").map_or(generic.len(), |i| i + 2);
        let (create, rest) = generic.split_at(end);
        let store = t.options.get(STORE).map(|s| s.trim().to_ascii_uppercase()).filter(|s| s == "ROW").unwrap_or("COLUMN".into());
        let create = create.replacen("CREATE TABLE", &format!("CREATE {store} TABLE"), 1);
        out.push(if parts.if_exists && !parts.drop {
            guarded(&create, false, "SYS.TABLES", "TABLE_NAME", schema, &t.name)
        } else {
            format!("{create};")
        });
        let rest = rest.trim_start_matches(';').trim();
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
    }

    if parts.indexes {
        for ix in &t.indexes {
            if crate::structure::is_fulltext(ix) {
                let s = crate::structure::fulltext_sql(&name, ix);
                out.push(if parts.if_exists { guarded(&s, false, "SYS.FULLTEXT_INDEXES", "INDEX_NAME", schema, &ix.name) } else { format!("{s};") });
                continue;
            }
            let kind = ix.kind.as_deref().map(|k| k.trim().to_ascii_uppercase()).filter(|k| INDEX_TYPES.contains(&k.as_str()));
            let cols: Vec<String> = ix.columns.iter().map(|c| quote(c)).collect();
            let s = format!(
                "CREATE {}{}INDEX {} ON {name} ({})",
                if ix.unique { "UNIQUE " } else { "" },
                kind.map(|k| format!("{k} ")).unwrap_or_default(),
                quote(&ix.name),
                cols.join(", ")
            );
            out.push(if parts.if_exists { guarded(&s, false, "SYS.INDEXES", "INDEX_NAME", schema, &ix.name) } else { format!("{s};") });
        }
    }

    if parts.foreign_keys {
        let fks = ddl::table_ddl(&f, t, DdlParts { foreign_keys: true, ..Default::default() });
        if !fks.is_empty() {
            out.push(fks);
        }
    }
    out.join("\n")
}

// ------------------------------------------------------------ schema sync

/// HANA's ALTER TABLE takes columns in parentheses: `ADD (c t …)`,
/// `ALTER (c t [DEFAULT x] [NOT] NULL)` with the whole new definition,
/// `DROP (c)`; `DROP PRIMARY KEY`. Unique indexes that back a UNIQUE
/// constraint are dropped as constraints.
pub fn sync_script(changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
    use dbine_driver::alter::{self, AlterStyle, ColumnAlter, TableChange};
    let lit = |s: &str| ddl::sql_literal(&flavor(), &Value::String(s.into()));
    let mut comments: Vec<String> = Vec::new();
    let mut unique_drops: Vec<(String, String)> = Vec::new();
    let changes: Vec<TableChange> = changes
        .iter()
        .map(|c| match c {
            TableChange::Alter { old, new } => {
                let schema = new.schema.as_deref().filter(|s| !s.is_empty());
                let name = dbine_driver::sql::qualified_name(Quote::Double, schema, &new.name);
                // Comments go with COMMENT ON: ALTER (…) doesn't carry them.
                let mut new = new.clone();
                for n in &mut new.columns {
                    if let Some(o) = old.columns.iter().find(|o| o.name.eq_ignore_ascii_case(&n.name)) {
                        if o.comment.as_deref().unwrap_or("") != n.comment.as_deref().unwrap_or("") {
                            comments.push(format!("COMMENT ON COLUMN {name}.{} IS {};", quote(&n.name), n.comment.as_deref().map(lit).unwrap_or_else(|| "NULL".into())));
                        }
                        n.comment = o.comment.clone();
                    }
                }
                for ix in old.indexes.iter().filter(|i| crate::structure::is_fulltext(i)) {
                    let plain = format!("DROP INDEX {};", dbine_driver::sql::qualified_name(Quote::Double, schema, &ix.name));
                    unique_drops.push((plain, format!("DROP FULLTEXT INDEX {};", dbine_driver::sql::qualified_name(Quote::Double, schema, &ix.name))));
                }
                for ix in old.indexes.iter().filter(|i| i.unique) {
                    let owner = schema.map_or("CURRENT_SCHEMA".to_string(), lit);
                    unique_drops.push((
                        format!("DROP INDEX {};", dbine_driver::sql::qualified_name(Quote::Double, schema, &ix.name)),
                        format!(
                            "DO BEGIN\n  DECLARE n INTEGER;\n  SELECT COUNT(*) INTO n FROM SYS.CONSTRAINTS WHERE SCHEMA_NAME = {owner} AND TABLE_NAME = {} AND CONSTRAINT_NAME = {};\n  IF :n > 0 THEN\n    EXEC {};\n  ELSE\n    EXEC {};\n  END IF;\nEND;",
                            lit(&new.name),
                            lit(&ix.name),
                            lit(&format!("ALTER TABLE {name} DROP CONSTRAINT {}", quote(&ix.name))),
                            lit(&format!("DROP INDEX {}", dbine_driver::sql::qualified_name(Quote::Double, schema, &ix.name)))
                        ),
                    ));
                }
                TableChange::Alter { old: old.clone(), new }
            }
            other => other.clone(),
        })
        .collect();
    let f = flavor();
    let cd = |t: &TableSchema, c: &ColumnDef| ddl::column_def(&f, t, c);
    let dd = |t: &TableSchema, p: DdlParts| Ok(table_ddl(t, p));
    let mut st = AlterStyle::from_flavor(&f, ColumnAlter::Modify { keyword: "ALTER" }, &cd, &dd);
    st.add_column = "ADD";
    st.drop_pk_keyword = true;
    // COMMENT ON also for added columns (ALTER (…) doesn't carry comments).
    let comment_on = |t: &TableSchema, c: Option<&ColumnDef>, text: Option<&str>| {
        let name = dbine_driver::sql::qualified_name(Quote::Double, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
        let v = text.map(lit).unwrap_or_else(|| "NULL".into());
        Some(match c {
            Some(c) => format!("COMMENT ON COLUMN {name}.{} IS {v};", quote(&c.name)),
            None => format!("COMMENT ON TABLE {name} IS {v};"),
        })
    };
    let mut script = alter::sync_script_with_comments(&st, Some(&comment_on), &changes)?;
    for s in &mut script.statements {
        if let Some((_, block)) = unique_drops.iter().find(|(plain, _)| plain == s) {
            *s = block.clone();
            continue;
        }
        // `ALTER TABLE t ADD "c" …;` → `ADD ("c" …);`, same for ALTER and DROP COLUMN.
        let Some(rest) = s.strip_prefix("ALTER TABLE ") else { continue };
        for (from, to) in [("\" ADD \"", "\" ADD (\""), ("\" ALTER \"", "\" ALTER (\""), ("\" DROP COLUMN \"", "\" DROP (\"")] {
            if let Some(i) = rest.find(from) {
                let body = rest[i + from.len()..].trim_end_matches(';');
                // An identity can't be added to an existing column.
                let body = if from.contains("ALTER") { body.replace(" GENERATED BY DEFAULT AS IDENTITY", "") } else { body.to_string() };
                *s = format!("ALTER TABLE {}{to}{body});", &rest[..i]);
                break;
            }
        }
    }
    script.statements.extend(comments);
    Ok(script)
}

// --------------------------------------------------------------- designer

pub fn designer() -> DesignerSpec {
    let mut d = DesignerSpec::sql_table(vec![
        "INTEGER",
        "BIGINT",
        "SMALLINT",
        "TINYINT",
        "DECIMAL(18,2)",
        "DOUBLE",
        "REAL",
        "NVARCHAR(255)",
        "VARCHAR(255)",
        "NCLOB",
        "BLOB",
        "VARBINARY(256)",
        "BOOLEAN",
        "DATE",
        "TIME",
        "TIMESTAMP",
        "SECONDDATE",
    ]);
    d.comments = true;
    d.table_options = vec![Field::new(
        STORE,
        "Almacenamiento",
        FieldKind::Select(vec![("COLUMN", "Columnar (COLUMN)"), ("ROW", "Por filas (ROW)")]),
    )
    .default_value("COLUMN")];
    d
}

pub fn create_templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(kinds::VIEW, "Nueva vista", "CREATE VIEW {name} AS\nSELECT t.id, t.nombre\n  FROM tabla t\n WHERE t.activo = TRUE;\n"),
        t(
            kinds::PROCEDURE,
            "Nuevo procedimiento",
            "CREATE PROCEDURE {name} (\n    IN desde INTEGER,\n    OUT resultado TABLE (id INTEGER, nombre NVARCHAR(100))\n)\nLANGUAGE SQLSCRIPT\nSQL SECURITY INVOKER\nREADS SQL DATA AS\nBEGIN\n  resultado = SELECT id, nombre FROM tabla WHERE id >= :desde;\nEND;\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función",
            "CREATE FUNCTION {name} (x INTEGER)\nRETURNS resultado INTEGER\nLANGUAGE SQLSCRIPT\nSQL SECURITY INVOKER AS\nBEGIN\n  resultado = :x * 2;\nEND;\n",
        ),
        t(
            kinds::TRIGGER,
            "Nuevo trigger",
            "CREATE TRIGGER {name}\nBEFORE UPDATE ON tabla\nREFERENCING NEW ROW nueva\nFOR EACH ROW\nBEGIN\n  nueva.modificado = CURRENT_TIMESTAMP;\nEND;\n",
        ),
        t(kinds::SEQUENCE, "Nueva secuencia", "CREATE SEQUENCE {name} START WITH 1 INCREMENT BY 1;\n\n-- Siguiente valor:\n-- SELECT {name}.NEXTVAL FROM DUMMY;\n"),
        t(kinds::SYNONYM, "Nuevo sinónimo", "CREATE SYNONYM {name} FOR tabla;\n"),
        t(kinds::TYPE, "Nuevo tipo de tabla", "CREATE TYPE {name} AS TABLE (\n    id INTEGER NOT NULL,\n    nombre NVARCHAR(100)\n);\n"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            name: "PEDIDOS".into(),
            columns: vec![
                ColumnDef { name: "ID".into(), data_type: "BIGINT".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef { name: "CLIENTE_ID".into(), data_type: "INTEGER".into(), nullable: true, comment: Some("dueño".into()), ..Default::default() },
                ColumnDef { name: "ESTADO".into(), data_type: "NVARCHAR(20)".into(), nullable: false, default_value: Some("'nuevo'".into()), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["ID".into()] }),
            foreign_keys: vec![ForeignKeyDef {
                name: Some("FK_CLIENTE".into()),
                columns: vec!["CLIENTE_ID".into()],
                ref_table: "CLIENTES".into(),
                ref_columns: vec!["ID".into()],
                on_delete: Some("CASCADE".into()),
                ..Default::default()
            }],
            indexes: vec![
                IndexDef { name: "IX_ESTADO".into(), columns: vec!["ESTADO".into()], kind: Some("CPBTREE".into()), ..Default::default() },
                IndexDef { name: "UK_PEDIDOS_1".into(), columns: vec!["CLIENTE_ID".into(), "ESTADO".into()], unique: true, kind: Some("FULLTEXT?".into()), ..Default::default() },
            ],
            comment: Some("Pedidos".into()),
            ..Default::default()
        }
    }

    #[test]
    fn create_table() {
        let s = table_ddl(&table(), DdlParts { create: true, indexes: true, foreign_keys: true, ..Default::default() });
        assert!(s.starts_with("CREATE COLUMN TABLE \"PEDIDOS\" (\n    \"ID\" BIGINT GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n"), "{s}");
        assert!(s.contains("    \"CLIENTE_ID\" INTEGER NULL,\n"));
        assert!(s.contains("    \"ESTADO\" NVARCHAR(20) DEFAULT 'nuevo' NOT NULL,\n    PRIMARY KEY (\"ID\")\n);\n"));
        assert!(s.contains("COMMENT ON TABLE \"PEDIDOS\" IS 'Pedidos';\nCOMMENT ON COLUMN \"PEDIDOS\".\"CLIENTE_ID\" IS 'dueño';"));
        assert!(s.contains("CREATE CPBTREE INDEX \"IX_ESTADO\" ON \"PEDIDOS\" (\"ESTADO\");"));
        assert!(s.contains("CREATE UNIQUE INDEX \"UK_PEDIDOS_1\" ON \"PEDIDOS\" (\"CLIENTE_ID\", \"ESTADO\");"));
        assert!(s.ends_with(
            "ALTER TABLE \"PEDIDOS\" ADD CONSTRAINT \"FK_CLIENTE\" FOREIGN KEY (\"CLIENTE_ID\") REFERENCES \"CLIENTES\" (\"ID\") ON DELETE CASCADE;"
        ));
    }

    #[test]
    fn row_store_and_guards() {
        let mut t = table();
        t.options.insert(STORE.into(), "row".into());
        let s = table_ddl(&t, DdlParts { drop: true, if_exists: true, create: true, ..Default::default() });
        assert!(s.starts_with(
            "DO BEGIN\n  DECLARE n INTEGER;\n  SELECT COUNT(*) INTO n FROM SYS.TABLES WHERE SCHEMA_NAME = CURRENT_SCHEMA AND TABLE_NAME = 'PEDIDOS';\n  IF :n > 0 THEN\n    EXEC 'DROP TABLE \"PEDIDOS\"';\n  END IF;\nEND;\nCREATE ROW TABLE \"PEDIDOS\""
        ), "{s}");
        t.schema = Some("VENTAS".into());
        let s = table_ddl(&t, DdlParts { if_exists: true, create: true, indexes: true, ..Default::default() });
        assert!(s.contains("WHERE SCHEMA_NAME = 'VENTAS' AND TABLE_NAME = 'PEDIDOS';\n  IF :n = 0 THEN\n    EXEC 'CREATE ROW TABLE \"VENTAS\".\"PEDIDOS\" ("), "{s}");
        assert!(s.contains("DEFAULT ''nuevo'' NOT NULL"));
        assert!(s.contains("FROM SYS.INDEXES WHERE SCHEMA_NAME = 'VENTAS' AND INDEX_NAME = 'IX_ESTADO'"));
        // Guarded blocks are whole statements for the splitter.
        assert_eq!(crate::script::split(&s).len(), 1 + 2 + 2, "{s}");
    }

    #[test]
    fn defaults_and_names() {
        assert_eq!(default_literal("NVARCHAR(20)", "nuevo"), "'nuevo'");
        assert_eq!(default_literal("NVARCHAR(20)", "it's"), "'it''s'");
        assert_eq!(default_literal("TIMESTAMP", "CURRENT_TIMESTAMP"), "CURRENT_TIMESTAMP");
        assert_eq!(default_literal("DATE", "2024-01-01"), "'2024-01-01'");
        assert_eq!(default_literal("INTEGER", "0"), "0");
        assert_eq!(default_literal("BOOLEAN", "TRUE"), "TRUE");
        assert_eq!(user_name("_SYS_TREE_CS_#1_#0_#P0".into()), None);
        assert_eq!(rule(Some("RESTRICT".into())), None);
        assert_eq!(rule(Some("SET DEFAULT".into())).as_deref(), Some("SET DEFAULT"));
    }

    #[test]
    fn templates_cover_the_kinds() {
        let ts = create_templates();
        for k in crate::info().object_kinds.iter().map(|k| k.id).filter(|k| *k != kinds::TABLE) {
            assert!(ts.iter().any(|t| t.kind == k), "{k}");
        }
        for t in &ts {
            let n = crate::script::split(&t.template).len();
            assert_eq!(n, 1, "{}", t.template);
        }
    }
    #[test]
    fn sync_alters_columns_in_parentheses() {
        use dbine_driver::alter::TableChange;
        let old = table();
        let mut new = table();
        new.columns[2].data_type = "NVARCHAR(40)".into();
        new.columns[2].nullable = true;
        new.columns[1].comment = Some("cliente".into());
        new.columns.push(ColumnDef { name: "EMAIL".into(), data_type: "NVARCHAR(100)".into(), nullable: true, ..Default::default() });
        new.columns.retain(|c| c.name != "CLIENTE_ID");
        new.foreign_keys.clear();
        new.indexes.clear();
        new.primary_key.as_mut().unwrap().columns.push("ESTADO".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements[0], "ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"FK_CLIENTE\";");
        assert_eq!(s.statements[1], "DROP INDEX \"IX_ESTADO\";");
        assert!(s.statements[2].starts_with("DO BEGIN\n  DECLARE n INTEGER;\n  SELECT COUNT(*) INTO n FROM SYS.CONSTRAINTS WHERE SCHEMA_NAME = CURRENT_SCHEMA AND TABLE_NAME = 'PEDIDOS' AND CONSTRAINT_NAME = 'UK_PEDIDOS_1';"), "{}", s.statements[2]);
        assert!(s.statements[2].contains("EXEC 'ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"UK_PEDIDOS_1\"';\n  ELSE\n    EXEC 'DROP INDEX \"UK_PEDIDOS_1\"';"));
        assert_eq!(
            &s.statements[3..],
            [
                "ALTER TABLE \"PEDIDOS\" DROP PRIMARY KEY;",
                "ALTER TABLE \"PEDIDOS\" DROP (\"CLIENTE_ID\");",
                "ALTER TABLE \"PEDIDOS\" ADD (\"EMAIL\" NVARCHAR(100) NULL);",
                "ALTER TABLE \"PEDIDOS\" ALTER (\"ESTADO\" NVARCHAR(40) DEFAULT 'nuevo' NOT NULL);",
                "ALTER TABLE \"PEDIDOS\" ADD PRIMARY KEY (\"ID\", \"ESTADO\");",
            ]
        );
    }

    #[test]
    fn fulltext_indexes_and_checks() {
        use dbine_driver::alter::TableChange;
        use dbine_driver::CheckDef;
        let mut t = table();
        t.checks.push(CheckDef { name: Some("CK_ESTADO".into()), expression: "\"ESTADO\" IN ('nuevo', 'cerrado')".into() });
        t.indexes.push(IndexDef {
            name: "FTI_ESTADO".into(),
            columns: vec!["ESTADO".into()],
            kind: Some("FULLTEXT".into()),
            options: [("FUZZY_SEARCH_INDEX".to_string(), "TRUE".to_string())].into(),
            ..Default::default()
        });
        let s = table_ddl(&t, DdlParts { create: true, indexes: true, ..Default::default() });
        assert!(s.contains("    CONSTRAINT \"CK_ESTADO\" CHECK (\"ESTADO\" IN ('nuevo', 'cerrado'))\n);"), "{s}");
        assert!(s.contains("CREATE FULLTEXT INDEX \"FTI_ESTADO\" ON \"PEDIDOS\" (\"ESTADO\") FUZZY SEARCH INDEX ON;"), "{s}");
        let g = table_ddl(&t, DdlParts { if_exists: true, indexes: true, ..Default::default() });
        assert!(g.contains("FROM SYS.FULLTEXT_INDEXES WHERE SCHEMA_NAME = CURRENT_SCHEMA AND INDEX_NAME = 'FTI_ESTADO'"), "{g}");

        // The index's settings changed and the CHECK went: dropped as a full-text index.
        let mut new = t.clone();
        new.indexes.last_mut().unwrap().options.clear();
        new.checks.clear();
        let s = sync_script(&[TableChange::Alter { old: t.clone(), new }]).unwrap();
        assert_eq!(
            s.statements,
            [
                "DROP FULLTEXT INDEX \"FTI_ESTADO\";",
                "ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"CK_ESTADO\";",
                "CREATE FULLTEXT INDEX \"FTI_ESTADO\" ON \"PEDIDOS\" (\"ESTADO\");",
            ]
        );
    }

    #[test]
    fn sync_comment_only_changes_use_comment_on() {
        use dbine_driver::alter::TableChange;
        let old = table();
        let mut new = table();
        new.columns[1].comment = Some("cliente".into());
        new.columns[2].nullable = true;
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            [
                "ALTER TABLE \"PEDIDOS\" ALTER (\"ESTADO\" NVARCHAR(20) DEFAULT 'nuevo' NULL);",
                "COMMENT ON COLUMN \"PEDIDOS\".\"CLIENTE_ID\" IS 'cliente';",
            ]
        );
        // An added column's comment and the table's.
        let old = table();
        let mut new = table();
        new.comment = Some("Pedidos 2".into());
        new.columns.push(ColumnDef { name: "NOTA".into(), data_type: "NVARCHAR(50)".into(), nullable: true, comment: Some("nota".into()), ..Default::default() });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            [
                "ALTER TABLE \"PEDIDOS\" ADD (\"NOTA\" NVARCHAR(50) NULL);",
                "COMMENT ON COLUMN \"PEDIDOS\".\"NOTA\" IS 'nota';",
                "COMMENT ON TABLE \"PEDIDOS\" IS 'Pedidos 2';",
            ]
        );
    }
}
