//! Tables as [`TableSchema`] from the dictionary views, and Oracle DDL /
//! INSERT scripts from them.
//!
//! Scripts are meant for `execute`'s splitter (`script.rs`): plain SQL ends
//! in `;`, PL/SQL guard blocks end in a `/` line.

use crate::{err, format_type, quote, PACKAGE};
use dbine_driver::ddl::{self, AutoIncrement, SqlFlavor};
use dbine_driver::sql::Quote;
use dbine_driver::{
    kinds, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, ForeignKeyDef, IndexDef, KeyDef,
    Result, TableSchema,
};
use oracledb::Connection;
use std::collections::HashMap;

pub const FLAVOR: SqlFlavor = SqlFlavor {
    quote: Quote::Double,
    auto_increment: AutoIncrement::GeneratedIdentity,
    comment_on: true,
    inline_comments: false,
    // No IF [NOT] EXISTS before 23ai: PL/SQL guards instead.
    if_exists: false,
    fk_inline: false,
    multi_row_insert: false,
    true_literal: "1",
    false_literal: "0",
};

/// `IndexDef::kind` of a unique constraint (as opposed to a unique index):
/// it's recreated as a constraint, so foreign keys can reference it.
pub const UNIQUE_CONSTRAINT: &str = "CONSTRAINT";

// ------------------------------------------------------------- catalog

const TABLES: &str = "SELECT t.table_name, c.comments
   FROM all_tables t
   LEFT JOIN all_tab_comments c ON c.owner = t.owner AND c.table_name = t.table_name AND c.table_type = 'TABLE'
  WHERE t.owner = :1
    AND t.table_name NOT LIKE 'BIN$%' AND t.dropped = 'NO' AND t.nested = 'NO' AND t.secondary = 'N'
    AND (t.iot_type IS NULL OR t.iot_type = 'IOT')
    AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner AND m.mview_name = t.table_name)
    AND NOT EXISTS (SELECT 1 FROM all_mview_logs l WHERE l.log_owner = t.owner AND l.log_table = t.table_name)
  ORDER BY t.table_name";

const COLUMNS: &str = "SELECT c.table_name, c.column_name, c.data_type, c.char_length, c.data_length,
        c.data_precision, c.data_scale, c.nullable, c.identity_column, c.virtual_column, m.comments, c.char_used, c.data_default
   FROM all_tab_cols c
   LEFT JOIN all_col_comments m ON m.owner = c.owner AND m.table_name = c.table_name AND m.column_name = c.column_name
  WHERE c.owner = :1 AND c.hidden_column = 'NO'
  ORDER BY c.table_name, c.column_id";

/// Primary keys, unique constraints and foreign keys, column by column,
/// with the referenced columns matched by position.
const CONSTRAINTS: &str = "SELECT c.table_name, c.constraint_name, c.constraint_type, c.generated, c.delete_rule,
        cc.column_name, r.owner, r.table_name, rc.column_name
   FROM all_constraints c
   JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name AND cc.table_name = c.table_name
   LEFT JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name
   LEFT JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name
                                AND rc.position = cc.position
  WHERE c.owner = :1 AND c.constraint_type IN ('P', 'U', 'R') AND c.table_name NOT LIKE 'BIN$%'
  ORDER BY c.table_name, c.constraint_name, cc.position";

/// Plain and bitmap (also function-based) indexes, without the ones backing
/// a primary key / unique constraint (those come as constraints).
const INDEXES: &str = "SELECT i.table_name, i.index_name, i.uniqueness, i.index_type, ic.column_name, ic.descend,
        ic.column_position
   FROM all_indexes i
   JOIN all_ind_columns ic ON ic.index_owner = i.owner AND ic.index_name = i.index_name
  WHERE i.table_owner = :1 AND i.generated = 'N'
    AND i.index_type IN ('NORMAL', 'NORMAL/REV', 'BITMAP', 'FUNCTION-BASED NORMAL', 'FUNCTION-BASED BITMAP',
                         'DOMAIN', 'FUNCTION-BASED DOMAIN')
    AND NOT EXISTS (SELECT 1 FROM all_constraints k
                     WHERE k.owner = i.table_owner AND k.table_name = i.table_name
                       AND k.index_owner = i.owner AND k.index_name = i.index_name AND k.constraint_type IN ('P', 'U'))
  ORDER BY i.table_name, i.index_name, ic.column_position";

/// Function-based / descending index columns (`SYS_NC…$` in ALL_IND_COLUMNS).
const INDEX_EXPRESSIONS: &str = "SELECT index_name, column_position, column_expression
   FROM all_ind_expressions WHERE table_owner = :1";

/// Every user table of `owner` with columns, keys, indexes and comments.
pub fn load_schema(c: &Connection, owner: &str) -> Result<Vec<TableSchema>> {
    let mut tables: Vec<TableSchema> = Vec::new();
    for row in c.query(TABLES, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        tables.push(TableSchema {
            kind: kinds::TABLE.into(),
            name: row.get(0).map_err(err)?,
            comment: row.get(1).map_err(err)?,
            ..Default::default()
        });
    }
    let at: HashMap<String, usize> = tables.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();

    for row in c.query(COLUMNS, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        let table: String = row.get(0).map_err(err)?;
        let Some(&i) = at.get(&table) else { continue };
        let ty: String = row.get(2).map_err(err)?;
        let mut data_type = crate::char_semantics(
            format_type(&ty, row.get(3).map_err(err)?, row.get(4).map_err(err)?, row.get(5).map_err(err)?, row.get(6).map_err(err)?),
            &ty,
            row.get::<Option<String>>(11).map_err(err)?.as_deref(),
        );
        let nullable: Option<String> = row.get(7).map_err(err)?;
        let identity = row.get::<Option<String>>(8).map_err(err)?.as_deref() == Some("YES");
        let is_virtual = row.get::<Option<String>>(9).map_err(err)?.as_deref() == Some("YES");
        // DATA_DEFAULT is a LONG: last in the select list.
        let mut default: Option<String> =
            row.get::<Option<String>>(12).map_err(err)?.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        if identity {
            // The identity's own sequence (`"X"."ISEQ$$_…".nextval`).
            default = None;
        } else if is_virtual {
            data_type = format!("{data_type} GENERATED ALWAYS AS ({}) VIRTUAL", default.take().unwrap_or_default());
        }
        tables[i].columns.push(ColumnDef {
            name: row.get(1).map_err(err)?,
            data_type,
            nullable: nullable.as_deref() != Some("N"),
            default_value: default,
            auto_increment: identity,
            comment: row.get(10).map_err(err)?,
            ..Default::default()
        });
    }

    // (table, constraint) of the key being built.
    let mut last: Option<(usize, String)> = None;
    for row in c.query(CONSTRAINTS, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        let table: String = row.get(0).map_err(err)?;
        let Some(&i) = at.get(&table) else { continue };
        let name: String = row.get(1).map_err(err)?;
        let ty: String = row.get(2).map_err(err)?;
        let generated = row.get::<Option<String>>(3).map_err(err)?.as_deref() == Some("GENERATED NAME");
        let column: String = row.get(5).map_err(err)?;
        let first = last.as_ref() != Some(&(i, name.clone()));
        last = Some((i, name.clone()));
        // System names (SYS_C…) are left for the target database to pick.
        let shown = (!generated).then(|| name.clone());
        let t = &mut tables[i];
        match ty.as_str() {
            "P" => t.primary_key.get_or_insert_with(|| KeyDef { name: shown, columns: vec![] }).columns.push(column),
            "U" => {
                if first {
                    t.indexes.push(IndexDef {
                        name: name.clone(),
                        columns: vec![],
                        unique: true,
                        kind: Some(UNIQUE_CONSTRAINT.into()),
                        filter: None,
                        ..Default::default()
                    });
                }
                t.indexes.last_mut().expect("pushed").columns.push(column);
            }
            _ => {
                if first {
                    let ref_owner: Option<String> = row.get(6).map_err(err)?;
                    let rule: Option<String> = row.get(4).map_err(err)?;
                    t.foreign_keys.push(ForeignKeyDef {
                        name: shown,
                        columns: vec![],
                        ref_schema: ref_owner.filter(|o| o != owner),
                        ref_table: row.get::<Option<String>>(7).map_err(err)?.unwrap_or_default(),
                        ref_columns: vec![],
                        on_delete: rule.filter(|r| r != "NO ACTION"),
                        on_update: None,
                    });
                }
                let fk = t.foreign_keys.last_mut().expect("pushed");
                fk.columns.push(column);
                fk.ref_columns.push(row.get::<Option<String>>(8).map_err(err)?.unwrap_or_default());
            }
        }
    }

    let mut expressions: HashMap<(String, i64), String> = HashMap::new();
    for row in c.query(INDEX_EXPRESSIONS, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        if let Some(e) = row.get::<Option<String>>(2).map_err(err)? {
            expressions.insert((row.get(0).map_err(err)?, row.get(1).map_err(err)?), e.trim().to_string());
        }
    }
    let mut last: Option<(usize, String)> = None;
    for row in c.query(INDEXES, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        let table: String = row.get(0).map_err(err)?;
        let Some(&i) = at.get(&table) else { continue };
        let name: String = row.get(1).map_err(err)?;
        if last.as_ref() != Some(&(i, name.clone())) {
            let ty: String = row.get(3).map_err(err)?;
            tables[i].indexes.push(IndexDef {
                name: name.clone(),
                columns: vec![],
                unique: row.get::<Option<String>>(2).map_err(err)?.as_deref() == Some("UNIQUE"),
                kind: ty.ends_with("BITMAP").then(|| "BITMAP".into()),
                filter: None,
                ..Default::default()
            });
            last = Some((i, name.clone()));
        }
        let column: String = row.get(4).map_err(err)?;
        let desc = row.get::<Option<String>>(5).map_err(err)?.as_deref() == Some("DESC");
        let position: i64 = row.get(6).map_err(err)?;
        // Expressions (and DESC columns, stored as `"COL"`) are kept as SQL:
        // `index_column` writes them as they are.
        let column = match expressions.get(&(name, position)) {
            Some(e) if desc => format!("{e} DESC"),
            Some(e) => e.clone(),
            None => column,
        };
        tables[i].indexes.last_mut().expect("pushed").columns.push(column);
    }
    for t in &mut tables {
        t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    }
    crate::structure::complete(c, owner, &mut tables)?;
    Ok(tables)
}

// ------------------------------------------------------------------- DDL

/// An index column: a name, or an expression (`UPPER("NAME")`, `"X" DESC`)
/// as the dictionary gives it.
fn index_column(c: &str) -> String {
    if c.contains(['"', '(']) {
        c.to_string()
    } else {
        quote(c)
    }
}

/// `stmt` (without its `;`) in a PL/SQL block that ignores the given
/// ORA- codes, ending in a `/` line.
fn guarded(stmt: &str, codes: &[i32]) -> String {
    let cond = codes.iter().map(|c| format!("SQLCODE != {c}")).collect::<Vec<_>>().join(" AND ");
    format!(
        "BEGIN\n  EXECUTE IMMEDIATE '{}';\nEXCEPTION\n  WHEN OTHERS THEN\n    IF {cond} THEN RAISE; END IF;\nEND;\n/",
        stmt.replace('\'', "''")
    )
}

/// `stmt;`, or guarded against `codes` when `guard`.
fn statement(stmt: String, guard: bool, codes: &[i32]) -> String {
    if guard {
        guarded(&stmt, codes)
    } else {
        format!("{stmt};")
    }
}

pub fn table_ddl(table: &TableSchema, parts: DdlParts) -> String {
    let mut t = table.clone();
    for c in t.columns.iter_mut().filter(|c| c.auto_increment) {
        // Identity columns are NOT NULL and take no DEFAULT.
        c.default_value = None;
        c.nullable = false;
    }
    let name = dbine_driver::sql::qualified_name(Quote::Double, t.schema.as_deref(), &t.name);
    let mut out: Vec<String> = Vec::new();

    if parts.drop {
        // ORA-00942: table or view does not exist.
        out.push(statement(format!("DROP TABLE {name} CASCADE CONSTRAINTS"), parts.if_exists, &[-942]));
    }

    if parts.create {
        let body = ddl::table_ddl(&FLAVOR, &t, DdlParts { create: true, ..Default::default() });
        // The CREATE TABLE, then the COMMENT ON statements.
        let (create, comments) = body.split_once("\n);").unwrap_or((&body, ""));
        let mut create = format!("{create}\n)");
        if let Some(ts) = t.options.get("tablespace").map(|s| s.trim()).filter(|s| !s.is_empty()) {
            create.push_str(&format!(" TABLESPACE {}", quote(ts)));
        }
        // ORA-00955: name is already used by an existing object.
        out.push(statement(create, parts.if_exists && !parts.drop, &[-955]));
        let comments = comments.trim();
        if !comments.is_empty() {
            out.push(comments.to_string());
        }
    }

    if parts.indexes {
        for ix in &t.indexes {
            let cols: Vec<String> = ix.columns.iter().map(|c| index_column(c)).collect();
            let s = if ix.kind.as_deref() == Some(UNIQUE_CONSTRAINT) {
                format!("ALTER TABLE {name} ADD CONSTRAINT {} UNIQUE ({})", quote(&ix.name), cols.join(", "))
            } else {
                crate::structure::index_sql(&name, ix, &cols)
            };
            // Name in use, same columns already indexed / unique, constraint name in use.
            out.push(statement(s, parts.if_exists, &[-955, -1408, -2261, -2264]));
        }
    }

    if parts.foreign_keys {
        for fk in &t.foreign_keys {
            // Oracle has no ON UPDATE, nor ON DELETE other than CASCADE / SET NULL.
            let fk = ForeignKeyDef {
                on_update: None,
                on_delete: fk.on_delete.clone().filter(|r| matches!(r.to_ascii_uppercase().as_str(), "CASCADE" | "SET NULL")),
                ..fk.clone()
            };
            let one = TableSchema { name: t.name.clone(), schema: t.schema.clone(), foreign_keys: vec![fk], ..Default::default() };
            let s = ddl::table_ddl(&FLAVOR, &one, DdlParts { foreign_keys: true, ..Default::default() });
            // Same foreign key already there, constraint name in use.
            out.push(statement(s.trim_end_matches(';').to_string(), parts.if_exists, &[-2275, -2264]));
        }
    }

    out.join("\n")
}

/// Identity columns as Oracle keeps them: NOT NULL, no DEFAULT (the
/// catalog reports the sequence's `nextval`, which differs per database).
fn normalize(t: &TableSchema) -> TableSchema {
    let mut t = t.clone();
    for c in t.columns.iter_mut().filter(|c| c.auto_increment) {
        c.default_value = None;
        c.nullable = false;
    }
    t
}

/// `ALTER TABLE … MODIFY (…)` for columns, `DROP PRIMARY KEY`, and unique
/// constraints dropped as constraints (`DROP INDEX` refuses them).
pub fn sync_script(changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
    use dbine_driver::alter::{self, AlterStyle, ColumnAlter, TableChange};
    let changes: Vec<TableChange> = changes
        .iter()
        .map(|c| match c {
            TableChange::Create { table } => TableChange::Create { table: normalize(table) },
            TableChange::Drop { table } => TableChange::Drop { table: normalize(table) },
            TableChange::Alter { old, new } => TableChange::Alter { old: normalize(old), new: normalize(new) },
        })
        .collect();
    let cd = |t: &TableSchema, c: &ColumnDef| ddl::column_def(&FLAVOR, t, c);
    let dd = |t: &TableSchema, p: DdlParts| Ok(table_ddl(t, p));
    let mut st = AlterStyle::from_flavor(&FLAVOR, ColumnAlter::Oracle, &cd, &dd);
    st.add_column = "ADD";
    st.drop_pk_keyword = true;
    let mut script = alter::sync_script(&st, &changes)?;
    // The planner drops every index with DROP INDEX; unique constraints go with DROP CONSTRAINT.
    for ch in &changes {
        if let TableChange::Alter { old, new } = ch {
            let name = dbine_driver::sql::qualified_name(Quote::Double, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
            for ix in old.indexes.iter().filter(|ix| ix.kind.as_deref() == Some(UNIQUE_CONSTRAINT)) {
                let plain = format!("DROP INDEX {};", dbine_driver::sql::qualified_name(Quote::Double, new.schema.as_deref().filter(|s| !s.is_empty()), &ix.name));
                for s in script.statements.iter_mut().filter(|s| **s == plain) {
                    *s = format!("ALTER TABLE {name} DROP CONSTRAINT {};", quote(&ix.name));
                }
            }
        }
    }
    Ok(script)
}

// -------------------------------------------------------------- designer

pub fn designer() -> DesignerSpec {
    let mut d = DesignerSpec::sql_table(vec![
        "NUMBER",
        "NUMBER(10)",
        "NUMBER(10,2)",
        "INTEGER",
        "VARCHAR2(50)",
        "VARCHAR2(255)",
        "VARCHAR2(4000)",
        "NVARCHAR2(255)",
        "CHAR(1)",
        "DATE",
        "TIMESTAMP",
        "TIMESTAMP WITH TIME ZONE",
        "CLOB",
        "NCLOB",
        "BLOB",
        "RAW(16)",
        "FLOAT",
        "BINARY_DOUBLE",
    ]);
    d.comments = true;
    d.table_options = vec![Field::new("tablespace", "Tablespace", FieldKind::Text)
        .placeholder("USERS")
        .help("Opcional. Si queda vacío se usa el tablespace por defecto del esquema.")];
    d
}

pub fn create_templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(kinds::VIEW, "Nueva vista", "CREATE OR REPLACE VIEW {name} AS\nSELECT\n    t.id,\n    t.nombre\nFROM mi_tabla t;\n"),
        t(
            kinds::MATERIALIZED_VIEW,
            "Nueva vista materializada",
            "CREATE MATERIALIZED VIEW {name}\n  BUILD IMMEDIATE\n  REFRESH COMPLETE ON DEMAND\nAS\nSELECT\n    t.id,\n    COUNT(*) AS total\nFROM mi_tabla t\nGROUP BY t.id;\n",
        ),
        t(
            kinds::PROCEDURE,
            "Nuevo procedimiento",
            "CREATE OR REPLACE PROCEDURE {name} (\n    p_id IN NUMBER\n) AS\nBEGIN\n    DBMS_OUTPUT.PUT_LINE('id: ' || p_id);\nEND {name};\n/\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función",
            "CREATE OR REPLACE FUNCTION {name} (\n    p_valor IN NUMBER\n) RETURN NUMBER AS\nBEGIN\n    RETURN p_valor * 2;\nEND {name};\n/\n",
        ),
        t(
            PACKAGE,
            "Nuevo paquete",
            "CREATE OR REPLACE PACKAGE {name} AS\n    PROCEDURE saludar(p_nombre IN VARCHAR2);\nEND {name};\n/\n\nCREATE OR REPLACE PACKAGE BODY {name} AS\n    PROCEDURE saludar(p_nombre IN VARCHAR2) IS\n    BEGIN\n        DBMS_OUTPUT.PUT_LINE('Hola, ' || p_nombre);\n    END saludar;\nEND {name};\n/\n",
        ),
        t(
            kinds::TRIGGER,
            "Nuevo trigger",
            "CREATE OR REPLACE TRIGGER {name}\n  BEFORE INSERT OR UPDATE ON mi_tabla\n  FOR EACH ROW\nBEGIN\n    :NEW.actualizado := SYSTIMESTAMP;\nEND;\n/\n",
        ),
        t(kinds::SEQUENCE, "Nueva secuencia", "CREATE SEQUENCE {name}\n  START WITH 1\n  INCREMENT BY 1\n  NOCACHE;\n"),
        t(kinds::SYNONYM, "Nuevo sinónimo", "CREATE OR REPLACE SYNONYM {name} FOR mi_tabla;\n"),
        t(
            kinds::TYPE,
            "Nuevo tipo",
            "CREATE OR REPLACE TYPE {name} AS OBJECT (\n    id     NUMBER,\n    nombre VARCHAR2(100)\n);\n/\n",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script;
    use serde_json::json;

    fn t() -> TableSchema {
        TableSchema {
            name: "PEDIDOS".into(),
            columns: vec![
                ColumnDef { name: "ID".into(), data_type: "NUMBER".into(), nullable: true, auto_increment: true, default_value: Some("\"X\".\"ISEQ$$_1\".nextval".into()), ..Default::default() },
                ColumnDef { name: "CLIENTE_ID".into(), data_type: "NUMBER(10)".into(), comment: Some("dueño".into()), ..Default::default() },
                ColumnDef { name: "ESTADO".into(), data_type: "VARCHAR2(20)".into(), nullable: false, default_value: Some("'nuevo'".into()), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: Some("PK_PEDIDOS".into()), columns: vec!["ID".into()] }),
            foreign_keys: vec![ForeignKeyDef {
                name: Some("FK_CLIENTE".into()),
                columns: vec!["CLIENTE_ID".into()],
                ref_table: "CLIENTES".into(),
                ref_columns: vec!["ID".into()],
                on_delete: Some("CASCADE".into()),
                on_update: Some("CASCADE".into()),
                ..Default::default()
            }],
            indexes: vec![
                IndexDef { name: "IX_ESTADO".into(), columns: vec!["ESTADO".into()], ..Default::default() },
                IndexDef { name: "IX_UP".into(), columns: vec!["UPPER(\"ESTADO\")".into(), "\"ID\" DESC".into()], ..Default::default() },
                IndexDef { name: "UQ_X".into(), columns: vec!["CLIENTE_ID".into(), "ESTADO".into()], unique: true, kind: Some(UNIQUE_CONSTRAINT.into()), filter: None, ..Default::default() },
            ],
            comment: Some("Pedidos de 'clientes'".into()),
            ..Default::default()
        }
    }
    const ALL: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: true };

    #[test]
    fn create_indexes_and_foreign_keys() {
        let mut tt = t();
        tt.options.insert("tablespace".into(), "USERS".into());
        let s = table_ddl(&tt, ALL);
        assert!(s.starts_with("CREATE TABLE \"PEDIDOS\" (\n    \"ID\" NUMBER GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n"), "{s}");
        assert!(!s.contains("ISEQ$$"));
        assert!(s.contains("    \"ESTADO\" VARCHAR2(20) DEFAULT 'nuevo' NOT NULL,\n    CONSTRAINT \"PK_PEDIDOS\" PRIMARY KEY (\"ID\")\n) TABLESPACE \"USERS\";\n"), "{s}");
        assert!(s.contains("COMMENT ON TABLE \"PEDIDOS\" IS 'Pedidos de ''clientes''';"));
        assert!(s.contains("COMMENT ON COLUMN \"PEDIDOS\".\"CLIENTE_ID\" IS 'dueño';"));
        assert!(s.contains("CREATE INDEX \"IX_ESTADO\" ON \"PEDIDOS\" (\"ESTADO\");"));
        assert!(s.contains("CREATE INDEX \"IX_UP\" ON \"PEDIDOS\" (UPPER(\"ESTADO\"), \"ID\" DESC);"));
        assert!(s.contains("ALTER TABLE \"PEDIDOS\" ADD CONSTRAINT \"UQ_X\" UNIQUE (\"CLIENTE_ID\", \"ESTADO\");"));
        assert!(s.ends_with(
            "ALTER TABLE \"PEDIDOS\" ADD CONSTRAINT \"FK_CLIENTE\" FOREIGN KEY (\"CLIENTE_ID\") REFERENCES \"CLIENTES\" (\"ID\") ON DELETE CASCADE;"
        ), "{s}");
        assert!(!s.contains("ON UPDATE"));
        assert!(script::split(&s).len() == 7, "{:?}", script::split(&s));
    }

    #[test]
    fn guards_run_through_the_splitter() {
        let s = table_ddl(&t(), DdlParts { drop: true, if_exists: true, ..ALL });
        assert!(s.starts_with(
            "BEGIN\n  EXECUTE IMMEDIATE 'DROP TABLE \"PEDIDOS\" CASCADE CONSTRAINTS';\nEXCEPTION\n  WHEN OTHERS THEN\n    IF SQLCODE != -942 THEN RAISE; END IF;\nEND;\n/\nCREATE TABLE"
        ), "{s}");
        assert!(s.contains("EXECUTE IMMEDIATE 'CREATE INDEX \"IX_ESTADO\" ON \"PEDIDOS\" (\"ESTADO\")';"));
        assert!(s.contains("IF SQLCODE != -2275 AND SQLCODE != -2264 THEN RAISE;"));
        let st = script::split(&s);
        assert_eq!(st.len(), 8, "{st:?}");
        assert!(st[0].plsql && !st[1].plsql);

        let s = table_ddl(&t(), DdlParts { if_exists: true, create: true, ..Default::default() });
        assert!(s.contains("EXECUTE IMMEDIATE 'CREATE TABLE \"PEDIDOS\" (\n"), "{s}");
        assert!(s.contains("DEFAULT ''nuevo'' NOT NULL"));
        assert!(s.contains("IF SQLCODE != -955 THEN RAISE;"));
        assert_eq!(script::split(&s).len(), 3);

        assert_eq!(table_ddl(&t(), DdlParts { drop: true, ..Default::default() }), "DROP TABLE \"PEDIDOS\" CASCADE CONSTRAINTS;");
    }

    #[test]
    fn inserts_one_row_each_with_numeric_booleans() {
        let s = ddl::insert_script(&FLAVOR, Some("APP"), "T", &["A".into(), "B".into()], &[vec![json!(1), json!(true)], vec![json!("it's"), json!(false)]], 1);
        assert_eq!(s, "INSERT INTO \"APP\".\"T\" (\"A\", \"B\") VALUES (1, 1);\nINSERT INTO \"APP\".\"T\" (\"A\", \"B\") VALUES ('it''s', 0);");
        assert_eq!(script::split(&s).len(), 2);
    }

    #[test]
    fn templates_cover_every_object_kind() {
        let info = crate::drivers().remove(0);
        let kinds: Vec<_> = create_templates().iter().map(|t| t.kind).collect();
        for k in info.info().object_kinds.iter().map(|k| k.id).filter(|k| *k != kinds::TABLE) {
            assert!(kinds.contains(&k), "{k}");
        }
        let pkg = create_templates().into_iter().find(|t| t.kind == PACKAGE).unwrap();
        let st = script::split(&pkg.template.replace("{name}", "P"));
        assert_eq!(st.len(), 2);
        assert!(st.iter().all(|s| s.plsql));
    }
    #[test]
    fn sync_alters_with_modify() {
        use dbine_driver::alter::TableChange;
        let old = t();
        let mut new = t();
        new.columns[2].data_type = "VARCHAR2(40)".into();
        new.columns[2].nullable = true;
        new.columns[1].default_value = Some("0".into());
        new.columns.push(ColumnDef { name: "NOTA".into(), data_type: "VARCHAR2(100)".into(), nullable: true, ..Default::default() });
        new.indexes.retain(|i| i.name != "UQ_X" && i.name != "IX_UP");
        new.indexes[0].columns.push("CLIENTE_ID".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX \"IX_ESTADO\";",
                "DROP INDEX \"IX_UP\";",
                "ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"UQ_X\";",
                "ALTER TABLE \"PEDIDOS\" ADD \"NOTA\" VARCHAR2(100) NULL;",
                "ALTER TABLE \"PEDIDOS\" MODIFY (\"CLIENTE_ID\" DEFAULT 0);",
                "ALTER TABLE \"PEDIDOS\" MODIFY (\"ESTADO\" VARCHAR2(40));",
                "ALTER TABLE \"PEDIDOS\" MODIFY (\"ESTADO\" NULL);",
                "CREATE INDEX \"IX_ESTADO\" ON \"PEDIDOS\" (\"ESTADO\", \"CLIENTE_ID\");",
            ]
        );
    }

    #[test]
    fn sync_ignores_identity_defaults_and_drops_the_key_by_keyword() {
        use dbine_driver::alter::TableChange;
        let old = t();
        let mut new = t();
        new.columns[0].default_value = Some("\"Y\".\"ISEQ$$_9\".nextval".into());
        new.primary_key = Some(KeyDef { name: Some("PK_PEDIDOS".into()), columns: vec!["ID".into(), "ESTADO".into()] });
        new.columns.retain(|c| c.name != "CLIENTE_ID");
        new.foreign_keys.clear();
        new.indexes.clear();
        let s = sync_script(&[TableChange::Alter { old, new }, TableChange::Drop { table: TableSchema { name: "VIEJA".into(), ..Default::default() } }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"FK_CLIENTE\";",
                "DROP TABLE \"VIEJA\" CASCADE CONSTRAINTS;",
                "DROP INDEX \"IX_ESTADO\";",
                "DROP INDEX \"IX_UP\";",
                "ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"UQ_X\";",
                "ALTER TABLE \"PEDIDOS\" DROP PRIMARY KEY;",
                "ALTER TABLE \"PEDIDOS\" DROP COLUMN \"CLIENTE_ID\";",
                "ALTER TABLE \"PEDIDOS\" ADD CONSTRAINT \"PK_PEDIDOS\" PRIMARY KEY (\"ID\", \"ESTADO\");",
            ]
        );
    }
}
