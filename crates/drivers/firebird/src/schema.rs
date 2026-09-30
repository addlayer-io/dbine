//! Tables with their keys and indexes from the RDB$ catalog, and DDL for
//! them: the ER diagram, the script generator and the table designer.
//!
//! Firebird (up to 5) has no `IF [NOT] EXISTS` for tables or indexes, and a
//! column can't say `NULL`: guarded statements go through an `EXECUTE BLOCK`
//! and nullable columns just omit `NOT NULL`.

use crate::{field_type, int, q, text, Conn};
use dbine_driver::ddl::{self, AutoIncrement, SqlFlavor};
use dbine_driver::sql::Quote;
use dbine_driver::{
    kinds, CheckDef, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, Field, FieldKind, ForeignKeyDef, IndexDef,
    KeyDef, Result, TableSchema,
};
use rsfbclient_core::Column;
use serde_json::Value;
use std::collections::HashMap;

/// Table option: global temporary table (`ON COMMIT DELETE | PRESERVE ROWS`).
pub const TEMPORARY: &str = "temporary";
const ON_COMMIT_DELETE: &str = "ON COMMIT DELETE ROWS";
const ON_COMMIT_PRESERVE: &str = "ON COMMIT PRESERVE ROWS";

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

/// Persistent tables and global temporary ones (type 4: preserve rows, 5:
/// delete rows); views, external and virtual tables are left out.
const TABLES: &str = "
SELECT TRIM(RDB$RELATION_NAME), RDB$DESCRIPTION, COALESCE(RDB$RELATION_TYPE, 0)
  FROM RDB$RELATIONS
 WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$VIEW_BLR IS NULL AND COALESCE(RDB$RELATION_TYPE, 0) IN (0, 4, 5)
 ORDER BY RDB$RELATION_NAME";

const COLUMNS: &str = "
SELECT TRIM(rf.RDB$RELATION_NAME), TRIM(rf.RDB$FIELD_NAME), f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE,
       f.RDB$FIELD_LENGTH, f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE,
       COALESCE(rf.RDB$NULL_FLAG, f.RDB$NULL_FLAG, 0), COALESCE(rf.RDB$DEFAULT_SOURCE, f.RDB$DEFAULT_SOURCE),
       rf.RDB$IDENTITY_TYPE, f.RDB$COMPUTED_SOURCE, rf.RDB$DESCRIPTION
  FROM RDB$RELATION_FIELDS rf
  JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = rf.RDB$FIELD_SOURCE
  JOIN RDB$RELATIONS r ON r.RDB$RELATION_NAME = rf.RDB$RELATION_NAME
 WHERE COALESCE(r.RDB$SYSTEM_FLAG, 0) = 0 AND r.RDB$VIEW_BLR IS NULL
 ORDER BY rf.RDB$RELATION_NAME, rf.RDB$FIELD_POSITION";

/// Primary keys, unique constraints and foreign keys with their columns
/// (and, for foreign keys, the referenced table, columns and rules).
const CONSTRAINTS: &str = "
SELECT TRIM(rc.RDB$RELATION_NAME), TRIM(rc.RDB$CONSTRAINT_NAME), TRIM(rc.RDB$CONSTRAINT_TYPE),
       TRIM(s.RDB$FIELD_NAME), TRIM(ri.RDB$RELATION_NAME), TRIM(rs.RDB$FIELD_NAME),
       TRIM(ref.RDB$DELETE_RULE), TRIM(ref.RDB$UPDATE_RULE)
  FROM RDB$RELATION_CONSTRAINTS rc
  JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = rc.RDB$INDEX_NAME
  JOIN RDB$INDICES i ON i.RDB$INDEX_NAME = rc.RDB$INDEX_NAME
  LEFT JOIN RDB$INDICES ri ON ri.RDB$INDEX_NAME = i.RDB$FOREIGN_KEY
  LEFT JOIN RDB$INDEX_SEGMENTS rs ON rs.RDB$INDEX_NAME = i.RDB$FOREIGN_KEY AND rs.RDB$FIELD_POSITION = s.RDB$FIELD_POSITION
  LEFT JOIN RDB$REF_CONSTRAINTS ref ON ref.RDB$CONSTRAINT_NAME = rc.RDB$CONSTRAINT_NAME
 WHERE rc.RDB$CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY')
 ORDER BY rc.RDB$RELATION_NAME, rc.RDB$CONSTRAINT_NAME, s.RDB$FIELD_POSITION";

/// Indexes that don't back a constraint. `{condition}` is the partial index
/// predicate (Firebird 5) or NULL on older servers.
const INDEXES: &str = "
SELECT TRIM(i.RDB$RELATION_NAME), TRIM(i.RDB$INDEX_NAME), COALESCE(i.RDB$UNIQUE_FLAG, 0),
       COALESCE(i.RDB$INDEX_TYPE, 0), i.RDB$EXPRESSION_SOURCE, {condition}, TRIM(s.RDB$FIELD_NAME),
       COALESCE(i.RDB$INDEX_INACTIVE, 0)
  FROM RDB$INDICES i
  LEFT JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = i.RDB$INDEX_NAME
 WHERE COALESCE(i.RDB$SYSTEM_FLAG, 0) = 0
   AND NOT EXISTS (SELECT 1 FROM RDB$RELATION_CONSTRAINTS rc WHERE rc.RDB$INDEX_NAME = i.RDB$INDEX_NAME)
 ORDER BY i.RDB$RELATION_NAME, i.RDB$INDEX_NAME, s.RDB$FIELD_POSITION";

/// CHECK constraints with their source (`CHECK (…)`), from the trigger
/// that enforces them on INSERT (another one, same source, does UPDATE).
const CHECKS: &str = "
SELECT TRIM(rc.RDB$RELATION_NAME), TRIM(rc.RDB$CONSTRAINT_NAME), tr.RDB$TRIGGER_SOURCE
  FROM RDB$RELATION_CONSTRAINTS rc
  JOIN RDB$CHECK_CONSTRAINTS cc ON cc.RDB$CONSTRAINT_NAME = rc.RDB$CONSTRAINT_NAME
  JOIN RDB$TRIGGERS tr ON tr.RDB$TRIGGER_NAME = cc.RDB$TRIGGER_NAME AND tr.RDB$TRIGGER_TYPE = 1
 WHERE rc.RDB$CONSTRAINT_TYPE = 'CHECK'
 ORDER BY rc.RDB$RELATION_NAME, rc.RDB$CONSTRAINT_NAME";

/// `CHECK (a > 0)` → `(a > 0)`: what follows the keyword (5 characters,
/// which the sync relies on to find an unnamed CHECK by its source).
fn check_condition(src: &str) -> Option<String> {
    let src = src.trim();
    let rest = src.get(..5).filter(|k| k.eq_ignore_ascii_case("CHECK")).map_or(src, |_| &src[5..]);
    Some(rest.trim().to_string()).filter(|r| !r.is_empty())
}

/// Index option: the index is inactive (`ALTER INDEX … INACTIVE`).
pub const INACTIVE: &str = "inactive";

/// Text after a leading keyword (`DEFAULT 'x'` → `'x'`, `WHERE a > 0` → `a > 0`).
fn after_keyword(s: &str, kw: &str) -> Option<String> {
    let s = s.trim();
    let rest = match s.get(..kw.len()) {
        Some(head) if head.eq_ignore_ascii_case(kw) && s[kw.len()..].starts_with(char::is_whitespace) => &s[kw.len()..],
        _ => s,
    };
    Some(rest.trim().to_string()).filter(|r| !r.is_empty())
}

/// Constraint names Firebird made up (`INTEG_12`): left out so the DDL
/// lets the target database name them again.
fn user_name(n: String) -> Option<String> {
    (!n.starts_with("INTEG_")).then_some(n)
}

/// `RESTRICT` / `NO ACTION` are the default.
fn rule(r: Option<String>) -> Option<String> {
    r.filter(|r| !matches!(r.as_str(), "RESTRICT" | "NO ACTION" | ""))
}

pub fn database_schema(c: &mut Conn) -> Result<Vec<TableSchema>> {
    let mut tables: Vec<TableSchema> = Vec::new();
    let mut pos: HashMap<String, usize> = HashMap::new();
    for r in c.rows(TABLES, vec![])? {
        let name = r.first().and_then(text).unwrap_or_default();
        let mut t = TableSchema {
            kind: kinds::TABLE.into(),
            name: name.clone(),
            comment: r.get(1).and_then(text).filter(|s| !s.is_empty()),
            ..Default::default()
        };
        match r.get(2).and_then(int) {
            Some(4) => {
                t.options.insert(TEMPORARY.into(), ON_COMMIT_PRESERVE.into());
            }
            Some(5) => {
                t.options.insert(TEMPORARY.into(), ON_COMMIT_DELETE.into());
            }
            _ => {}
        }
        pos.insert(name, tables.len());
        tables.push(t);
    }
    let get = |r: &[Column], i: usize| r.get(i).and_then(text);

    for r in c.rows(COLUMNS, vec![])? {
        let Some(&t) = get(&r, 0).and_then(|n| pos.get(&n)) else { continue };
        let g = |i: usize| r.get(i).and_then(int);
        let computed = get(&r, 11).filter(|s| !s.is_empty());
        tables[t].columns.push(ColumnDef {
            name: get(&r, 1).unwrap_or_default(),
            data_type: match &computed {
                Some(expr) => format!("COMPUTED BY {expr}"),
                None => field_type(g(2), g(3), g(4), g(5), g(6), g(7)),
            },
            nullable: computed.is_some() || g(8).unwrap_or(0) == 0,
            default_value: get(&r, 9).and_then(|d| after_keyword(&d, "DEFAULT")),
            auto_increment: g(10).is_some(),
            comment: get(&r, 12).filter(|s| !s.is_empty()),
            ..Default::default()
        });
    }

    // Rows come ordered by table, constraint and position.
    for r in c.rows(CONSTRAINTS, vec![])? {
        let Some(&t) = get(&r, 0).and_then(|n| pos.get(&n)) else { continue };
        let (name, ty, col) = (get(&r, 1).unwrap_or_default(), get(&r, 2).unwrap_or_default(), get(&r, 3).unwrap_or_default());
        let table = &mut tables[t];
        match ty.as_str() {
            "PRIMARY KEY" => {
                table.primary_key.get_or_insert_with(|| KeyDef { name: user_name(name), columns: vec![] }).columns.push(col);
            }
            "UNIQUE" => match table.indexes.last_mut().filter(|i| i.name == name) {
                Some(ix) => ix.columns.push(col),
                None => table.indexes.push(IndexDef { name, columns: vec![col], unique: true, ..Default::default() }),
            },
            _ => {
                let fk_name = Some(name);
                let ref_col = get(&r, 5).unwrap_or_default();
                match table.foreign_keys.last_mut().filter(|f| f.name == fk_name) {
                    Some(fk) => {
                        fk.columns.push(col);
                        fk.ref_columns.push(ref_col);
                    }
                    None => table.foreign_keys.push(ForeignKeyDef {
                        name: fk_name,
                        columns: vec![col],
                        ref_schema: None,
                        ref_table: get(&r, 4).unwrap_or_default(),
                        ref_columns: vec![ref_col],
                        on_delete: rule(get(&r, 6)),
                        on_update: rule(get(&r, 7)),
                    }),
                }
            }
        }
    }
    // Generated FK names only served to group the columns.
    for t in &mut tables {
        for fk in &mut t.foreign_keys {
            fk.name = fk.name.take().and_then(user_name);
        }
    }

    // RDB$CONDITION_SOURCE (partial indexes) arrived in Firebird 5.
    let rows = match c.rows(&INDEXES.replace("{condition}", "i.RDB$CONDITION_SOURCE"), vec![]) {
        Ok(rows) => rows,
        Err(_) => c.rows(&INDEXES.replace("{condition}", "CAST(NULL AS VARCHAR(1))"), vec![])?,
    };
    for r in rows {
        let Some(&t) = get(&r, 0).and_then(|n| pos.get(&n)) else { continue };
        let name = get(&r, 1).unwrap_or_default();
        let table = &mut tables[t];
        let col = get(&r, 6);
        if let Some(ix) = table.indexes.last_mut().filter(|i| i.name == name) {
            ix.columns.extend(col);
            continue;
        }
        let desc = r.get(3).and_then(int) == Some(1);
        let expr = get(&r, 4).filter(|s| !s.is_empty());
        let kind = match (desc, expr.is_some()) {
            (false, false) => None,
            (true, false) => Some("DESC".to_string()),
            (false, true) => Some("COMPUTED".to_string()),
            (true, true) => Some("DESC COMPUTED".to_string()),
        };
        let mut options = std::collections::BTreeMap::new();
        if r.get(7).and_then(int) == Some(1) {
            options.insert(INACTIVE.to_string(), "true".to_string());
        }
        table.indexes.push(IndexDef {
            name,
            columns: expr.into_iter().chain(col).collect(),
            unique: r.get(2).and_then(int) == Some(1),
            kind,
            filter: get(&r, 5).and_then(|w| after_keyword(&w, "WHERE")),
            options,
            ..Default::default()
        });
    }

    // CHECKs: the condition without the keyword; generated names left out.
    for r in c.rows(CHECKS, vec![])? {
        let Some(&t) = get(&r, 0).and_then(|n| pos.get(&n)) else { continue };
        let Some(expression) = get(&r, 2).as_deref().and_then(check_condition) else { continue };
        tables[t].checks.push(CheckDef { name: get(&r, 1).and_then(user_name), expression });
    }
    Ok(tables)
}

// -------------------------------------------------------------------- DDL

/// `stmt` run only if the object named `name` exists (`exists`) or not,
/// looked up in `catalog`.`column`.
fn guarded(stmt: &str, exists: bool, catalog: &str, column: &str, name: &str) -> String {
    let lit = |s: &str| ddl::sql_literal(&flavor(), &Value::String(s.into()));
    format!(
        "EXECUTE BLOCK AS\nBEGIN\n  IF ({}EXISTS (SELECT 1 FROM {catalog} WHERE {column} = {})) THEN\n    EXECUTE STATEMENT {};\nEND;",
        if exists { "" } else { "NOT " },
        lit(name),
        lit(stmt)
    )
}

/// Firebird takes no `NULL` column constraint: nullable columns say nothing.
fn drop_null(line: &str) -> String {
    let (body, comma) = match line.strip_suffix(',') {
        Some(b) => (b, ","),
        None => (line, ""),
    };
    match body.strip_suffix(" NULL") {
        Some(b) if line.starts_with("    \"") && !b.ends_with(" NOT") => format!("{b}{comma}"),
        _ => line.to_string(),
    }
}

pub fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let f = flavor();
    let name = q(&t.name);
    let mut out: Vec<String> = Vec::new();

    if parts.drop {
        let drop = format!("DROP TABLE {name}");
        out.push(if parts.if_exists { guarded(&drop, true, "RDB$RELATIONS", "RDB$RELATION_NAME", &t.name) } else { format!("{drop};") });
    }

    if parts.create {
        let generic = ddl::table_ddl(&f, t, DdlParts { create: true, ..Default::default() });
        // CREATE TABLE … (\n…\n); then the COMMENT ON statements.
        let end = generic.find("\n);").map_or(generic.len(), |i| i + 2);
        let (create, rest) = generic.split_at(end);
        let mut create = create.lines().map(drop_null).collect::<Vec<_>>().join("\n");
        if let Some(on_commit) = t.options.get(TEMPORARY).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            create = create.replacen("CREATE TABLE", "CREATE GLOBAL TEMPORARY TABLE", 1);
            create.push(' ');
            create.push_str(on_commit);
        }
        out.push(if parts.if_exists && !parts.drop {
            guarded(&create, false, "RDB$RELATIONS", "RDB$RELATION_NAME", &t.name)
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
            let kind = ix.kind.as_deref().unwrap_or_default().to_ascii_uppercase();
            let body = if kind.contains("COMPUTED") {
                let expr = ix.columns.join(", ");
                let expr = expr.trim();
                if expr.starts_with('(') && expr.ends_with(')') { format!("COMPUTED BY {expr}") } else { format!("COMPUTED BY ({expr})") }
            } else {
                format!("({})", ix.columns.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "))
            };
            let mut s = format!(
                "CREATE {}{}INDEX {} ON {name} {body}",
                if ix.unique { "UNIQUE " } else { "" },
                if kind.contains("DESC") { "DESC " } else { "" },
                q(&ix.name)
            );
            if let Some(w) = ix.filter.as_deref().filter(|w| !w.trim().is_empty()) {
                s.push_str(&format!(" WHERE {w}"));
            }
            out.push(if parts.if_exists { guarded(&s, false, "RDB$INDICES", "RDB$INDEX_NAME", &ix.name) } else { format!("{s};") });
            if ix.options.get(INACTIVE).is_some_and(|v| v == "true") {
                out.push(format!("ALTER INDEX {} INACTIVE;", q(&ix.name)));
            }
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

const UNNAMED_PK: &str = "__dbine_pk__";
const UNNAMED_FK: &str = "__dbine_fk_";
const UNNAMED_CK: &str = "__dbine_ck_";

fn lit(s: &str) -> String {
    ddl::sql_literal(&flavor(), &Value::String(s.into()))
}

/// Drop the constraints `select` finds (one name per row), whatever
/// Firebird called them.
fn drop_found(table: &str, select: &str) -> String {
    format!(
        "EXECUTE BLOCK AS\n  DECLARE n VARCHAR(255);\nBEGIN\n  FOR {select} INTO :n DO\n    EXECUTE STATEMENT {} || n || '\"';\nEND;",
        lit(&format!("ALTER TABLE {} DROP CONSTRAINT \"", q(table)))
    )
}

/// Firebird's ALTER TABLE: `ADD <column>`, `DROP <column>`, `ALTER c TYPE`,
/// `ALTER c SET/DROP NOT NULL` (3.0+), `ALTER c SET/DROP DEFAULT`. Keys
/// with generated names (`INTEG_n`, which the catalog doesn't report) and
/// unique constraints are looked up and dropped in an `EXECUTE BLOCK`.
pub fn sync_script(changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
    use dbine_driver::alter::{self, AlterStyle, ColumnAlter, TableChange};
    // No schemas: bare names, as in the rest of the DDL.
    let bare = |t: &TableSchema| {
        let mut t = TableSchema { schema: None, ..t.clone() };
        for fk in &mut t.foreign_keys {
            fk.ref_schema = None;
        }
        t
    };
    let mut blocks: Vec<(String, String)> = Vec::new();
    let changes: Vec<TableChange> = changes
        .iter()
        .map(|c| match c {
            TableChange::Create { table } => TableChange::Create { table: bare(table) },
            TableChange::Drop { table } => TableChange::Drop { table: bare(table) },
            TableChange::Alter { old, new } => {
                let mut old = bare(old);
                let name = q(&old.name);
                if let Some(pk) = old.primary_key.as_mut().filter(|k| k.name.as_deref().is_none_or(str::is_empty)) {
                    pk.name = Some(UNNAMED_PK.into());
                    let find = format!(
                        "SELECT TRIM(RDB$CONSTRAINT_NAME) FROM RDB$RELATION_CONSTRAINTS WHERE RDB$RELATION_NAME = {} AND RDB$CONSTRAINT_TYPE = 'PRIMARY KEY'",
                        lit(&old.name)
                    );
                    blocks.push((format!("ALTER TABLE {name} DROP CONSTRAINT \"{UNNAMED_PK}\";"), drop_found(&old.name, &find)));
                }
                let table = old.name.clone();
                for (i, fk) in old.foreign_keys.iter_mut().enumerate().filter(|(_, f)| f.name.as_deref().is_none_or(str::is_empty)) {
                    let tag = format!("{UNNAMED_FK}{i}__");
                    fk.name = Some(tag.clone());
                    // By the first column and the referenced table.
                    let find = format!(
                        "SELECT TRIM(c.RDB$CONSTRAINT_NAME) FROM RDB$RELATION_CONSTRAINTS c \
JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = c.RDB$INDEX_NAME AND s.RDB$FIELD_POSITION = 0 \
JOIN RDB$REF_CONSTRAINTS r ON r.RDB$CONSTRAINT_NAME = c.RDB$CONSTRAINT_NAME \
JOIN RDB$RELATION_CONSTRAINTS u ON u.RDB$CONSTRAINT_NAME = r.RDB$CONST_NAME_UQ \
WHERE c.RDB$RELATION_NAME = {} AND c.RDB$CONSTRAINT_TYPE = 'FOREIGN KEY' AND s.RDB$FIELD_NAME = {} AND u.RDB$RELATION_NAME = {}",
                        lit(&table),
                        lit(fk.columns.first().map(String::as_str).unwrap_or_default()),
                        lit(&fk.ref_table)
                    );
                    blocks.push((format!("ALTER TABLE {name} DROP CONSTRAINT \"{tag}\";"), drop_found(&table, &find)));
                }
                // A CHECK without a name of its own (INTEG_n) that goes: found
                // by its source, which Firebird keeps as written.
                let keeps = |c: &CheckDef| new.checks.iter().any(|n| dbine_driver::alter::check_expr(&n.expression) == dbine_driver::alter::check_expr(&c.expression));
                let mut tagged = 0;
                for ck in old.checks.iter_mut().filter(|c| c.name.as_deref().is_none_or(str::is_empty)) {
                    if keeps(ck) {
                        continue;
                    }
                    let tag = format!("{UNNAMED_CK}{tagged}__");
                    tagged += 1;
                    ck.name = Some(tag.clone());
                    let find = format!(
                        "SELECT TRIM(rc.RDB$CONSTRAINT_NAME) FROM RDB$RELATION_CONSTRAINTS rc \
JOIN RDB$CHECK_CONSTRAINTS cc ON cc.RDB$CONSTRAINT_NAME = rc.RDB$CONSTRAINT_NAME \
JOIN RDB$TRIGGERS tr ON tr.RDB$TRIGGER_NAME = cc.RDB$TRIGGER_NAME AND tr.RDB$TRIGGER_TYPE = 1 \
WHERE rc.RDB$RELATION_NAME = {} AND rc.RDB$CONSTRAINT_TYPE = 'CHECK' AND TRIM(SUBSTRING(tr.RDB$TRIGGER_SOURCE FROM 6)) = {}",
                        lit(&table),
                        lit(ck.expression.trim())
                    );
                    blocks.push((format!("ALTER TABLE {name} DROP CONSTRAINT \"{tag}\";"), drop_found(&table, &find)));
                }
                // A unique index may be a UNIQUE constraint, which DROP INDEX refuses.
                for ix in old.indexes.iter().filter(|i| i.unique) {
                    let drop = format!(
                        "EXECUTE BLOCK AS\nBEGIN\n  IF (EXISTS (SELECT 1 FROM RDB$RELATION_CONSTRAINTS WHERE RDB$CONSTRAINT_NAME = {})) THEN\n    EXECUTE STATEMENT {};\n  ELSE\n    EXECUTE STATEMENT {};\nEND;",
                        lit(&ix.name),
                        lit(&format!("ALTER TABLE {name} DROP CONSTRAINT {}", q(&ix.name))),
                        lit(&format!("DROP INDEX {}", q(&ix.name)))
                    );
                    blocks.push((format!("DROP INDEX {};", q(&ix.name)), drop));
                }
                TableChange::Alter { old, new: bare(new) }
            }
        })
        .collect();
    let f = flavor();
    // Nullable columns say nothing (no NULL constraint in Firebird).
    let cd = |t: &TableSchema, c: &ColumnDef| {
        let d = ddl::column_def(&f, t, c);
        match d.strip_suffix(" NULL") {
            Some(b) if !b.ends_with(" NOT") => b.to_string(),
            _ => d,
        }
    };
    let dd = |t: &TableSchema, p: DdlParts| Ok(table_ddl(t, p));
    let mut st = AlterStyle::from_flavor(&f, ColumnAlter::Standard { set_data_type: false, using_cast: false }, &cd, &dd);
    st.add_column = "ADD";
    let mut script = alter::sync_script(&st, &changes)?;
    for s in &mut script.statements {
        if let Some((_, block)) = blocks.iter().find(|(plain, _)| plain == s) {
            *s = block.clone();
        } else if s.starts_with("ALTER TABLE ") {
            // `DROP c`, not `DROP COLUMN c`; computed columns take `COMPUTED BY` without TYPE.
            *s = s.replacen("\" DROP COLUMN \"", "\" DROP \"", 1).replacen(" TYPE COMPUTED BY ", " COMPUTED BY ", 1);
        }
    }
    Ok(script)
}

// --------------------------------------------------------------- designer

pub fn designer() -> DesignerSpec {
    let mut d = DesignerSpec::sql_table(vec![
        "INTEGER",
        "BIGINT",
        "SMALLINT",
        "INT128",
        "NUMERIC(18,2)",
        "DECIMAL(18,2)",
        "DOUBLE PRECISION",
        "FLOAT",
        "DECFLOAT(34)",
        "VARCHAR(255)",
        "CHAR(10)",
        "BLOB SUB_TYPE TEXT",
        "BLOB SUB_TYPE BINARY",
        "BOOLEAN",
        "DATE",
        "TIME",
        "TIMESTAMP",
        "TIMESTAMP WITH TIME ZONE",
    ]);
    d.comments = true;
    d.table_options = vec![Field::new(
        TEMPORARY,
        "Tabla temporal global",
        FieldKind::Select(vec![("", "No"), (ON_COMMIT_DELETE, "Sí, se vacía al confirmar"), (ON_COMMIT_PRESERVE, "Sí, dura la conexión")]),
    )
    .default_value("")];
    d
}

pub fn create_templates() -> Vec<CreateTemplate> {
    let t = |kind, label, template: &str| CreateTemplate { kind, label, template: template.to_string() };
    vec![
        t(kinds::VIEW, "Nueva vista", "CREATE VIEW {name} (id, nombre) AS\nSELECT t.id, t.nombre\n  FROM tabla t\n WHERE t.activo = TRUE;\n"),
        t(
            kinds::PROCEDURE,
            "Nuevo procedimiento",
            "CREATE OR ALTER PROCEDURE {name} (\n    desde INTEGER\n)\nRETURNS (\n    id INTEGER,\n    nombre VARCHAR(100)\n)\nAS\nBEGIN\n  FOR SELECT t.id, t.nombre\n        FROM tabla t\n       WHERE t.id >= :desde\n        INTO :id, :nombre\n  DO\n    SUSPEND;\nEND;\n",
        ),
        t(
            kinds::FUNCTION,
            "Nueva función",
            "CREATE OR ALTER FUNCTION {name} (\n    x INTEGER\n)\nRETURNS INTEGER\nAS\nBEGIN\n  RETURN x * 2;\nEND;\n",
        ),
        t(
            crate::PACKAGE,
            "Nuevo paquete",
            "CREATE OR ALTER PACKAGE {name}\nAS\nBEGIN\n  FUNCTION doble (x INTEGER) RETURNS INTEGER;\nEND;\n\nRECREATE PACKAGE BODY {name}\nAS\nBEGIN\n  FUNCTION doble (x INTEGER) RETURNS INTEGER\n  AS\n  BEGIN\n    RETURN x * 2;\n  END\nEND;\n",
        ),
        t(
            kinds::TRIGGER,
            "Nuevo trigger",
            "CREATE OR ALTER TRIGGER {name} FOR tabla\nACTIVE BEFORE INSERT OR UPDATE POSITION 0\nAS\nBEGIN\n  NEW.modificado = CURRENT_TIMESTAMP;\nEND;\n",
        ),
        t(kinds::SEQUENCE, "Nueva secuencia", "CREATE SEQUENCE {name} START WITH 1 INCREMENT BY 1;\n\n-- Siguiente valor:\n-- SELECT NEXT VALUE FOR {name} FROM RDB$DATABASE;\n"),
        t(kinds::TYPE, "Nuevo dominio", "CREATE DOMAIN {name} AS VARCHAR(120)\n    DEFAULT ''\n    NOT NULL\n    CHECK (VALUE = '' OR VALUE LIKE '%@%');\n"),
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
                ColumnDef { name: "ID".into(), data_type: "INTEGER".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef { name: "CLIENTE_ID".into(), data_type: "INTEGER".into(), nullable: true, comment: Some("dueño".into()), ..Default::default() },
                ColumnDef { name: "ESTADO".into(), data_type: "VARCHAR(20)".into(), nullable: false, default_value: Some("'nuevo'".into()), ..Default::default() },
                ColumnDef { name: "NOTA".into(), data_type: "VARCHAR(10)".into(), nullable: true, default_value: Some("NULL".into()), ..Default::default() },
                ColumnDef { name: "DOBLE".into(), data_type: "COMPUTED BY (ID * 2)".into(), nullable: true, ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: Some("PK_PEDIDOS".into()), columns: vec!["ID".into()] }),
            foreign_keys: vec![ForeignKeyDef {
                name: Some("FK_CLIENTE".into()),
                columns: vec!["CLIENTE_ID".into()],
                ref_table: "CLIENTES".into(),
                ref_columns: vec!["ID".into()],
                on_delete: Some("CASCADE".into()),
                ..Default::default()
            }],
            indexes: vec![
                IndexDef { name: "IX_ESTADO".into(), columns: vec!["ESTADO".into()], kind: Some("DESC".into()), ..Default::default() },
                IndexDef { name: "UQ_NOTA".into(), columns: vec!["NOTA".into()], unique: true, filter: Some("NOTA IS NOT NULL".into()), ..Default::default() },
                IndexDef { name: "IX_UP".into(), columns: vec!["(UPPER(ESTADO))".into()], kind: Some("COMPUTED".into()), ..Default::default() },
            ],
            comment: Some("Pedidos".into()),
            ..Default::default()
        }
    }

    const ALL: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: true };

    #[test]
    fn create_table() {
        let s = table_ddl(&table(), ALL);
        assert!(s.starts_with("CREATE TABLE \"PEDIDOS\" (\n    \"ID\" INTEGER GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n"), "{s}");
        assert!(s.contains("    \"CLIENTE_ID\" INTEGER,\n"), "{s}");
        assert!(s.contains("    \"ESTADO\" VARCHAR(20) DEFAULT 'nuevo' NOT NULL,\n"));
        assert!(s.contains("    \"NOTA\" VARCHAR(10) DEFAULT NULL,\n"));
        assert!(s.contains("    \"DOBLE\" COMPUTED BY (ID * 2),\n"));
        assert!(s.contains("    CONSTRAINT \"PK_PEDIDOS\" PRIMARY KEY (\"ID\")\n);\n"));
        assert!(s.contains("COMMENT ON TABLE \"PEDIDOS\" IS 'Pedidos';"));
        assert!(s.contains("COMMENT ON COLUMN \"PEDIDOS\".\"CLIENTE_ID\" IS 'dueño';"));
        assert!(s.contains("CREATE DESC INDEX \"IX_ESTADO\" ON \"PEDIDOS\" (\"ESTADO\");"));
        assert!(s.contains("CREATE UNIQUE INDEX \"UQ_NOTA\" ON \"PEDIDOS\" (\"NOTA\") WHERE NOTA IS NOT NULL;"));
        assert!(s.contains("CREATE INDEX \"IX_UP\" ON \"PEDIDOS\" COMPUTED BY (UPPER(ESTADO));"));
        assert!(s.ends_with(
            "ALTER TABLE \"PEDIDOS\" ADD CONSTRAINT \"FK_CLIENTE\" FOREIGN KEY (\"CLIENTE_ID\") REFERENCES \"CLIENTES\" (\"ID\") ON DELETE CASCADE;"
        ));
    }

    #[test]
    fn guards_and_temporary() {
        let s = table_ddl(&table(), DdlParts { drop: true, if_exists: true, create: true, ..Default::default() });
        assert!(s.starts_with(
            "EXECUTE BLOCK AS\nBEGIN\n  IF (EXISTS (SELECT 1 FROM RDB$RELATIONS WHERE RDB$RELATION_NAME = 'PEDIDOS')) THEN\n    EXECUTE STATEMENT 'DROP TABLE \"PEDIDOS\"';\nEND;\nCREATE TABLE"
        ), "{s}");
        let mut t = table();
        t.options.insert(TEMPORARY.into(), ON_COMMIT_DELETE.into());
        let s = table_ddl(&t, DdlParts { if_exists: true, create: true, indexes: true, ..Default::default() });
        assert!(s.starts_with("EXECUTE BLOCK AS\nBEGIN\n  IF (NOT EXISTS (SELECT 1 FROM RDB$RELATIONS WHERE RDB$RELATION_NAME = 'PEDIDOS')) THEN\n    EXECUTE STATEMENT 'CREATE GLOBAL TEMPORARY TABLE \"PEDIDOS\" ("), "{s}");
        assert!(s.contains("\n) ON COMMIT DELETE ROWS';\nEND;"), "{s}");
        assert!(s.contains("DEFAULT ''nuevo'' NOT NULL"));
        assert!(s.contains("RDB$INDICES WHERE RDB$INDEX_NAME = 'IX_ESTADO'"));
        // Every guarded block is one statement for the splitter.
        let n = crate::script::split(&s).len();
        assert_eq!(n, 1 + 2 + 3, "{s}");
    }

    #[test]
    fn keywords_and_names() {
        assert_eq!(after_keyword("DEFAULT 'x'", "DEFAULT").as_deref(), Some("'x'"));
        assert_eq!(after_keyword("default CURRENT_TIMESTAMP", "DEFAULT").as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(after_keyword("where p1 > 0", "WHERE").as_deref(), Some("p1 > 0"));
        assert_eq!(after_keyword("DEFAULTS", "DEFAULT").as_deref(), Some("DEFAULTS"));
        assert_eq!(user_name("INTEG_4".into()), None);
        assert_eq!(rule(Some("RESTRICT".into())), None);
        assert_eq!(rule(Some("SET NULL".into())).as_deref(), Some("SET NULL"));
    }

    #[test]
    fn templates_cover_the_kinds() {
        let kinds: Vec<_> = create_templates().iter().map(|t| t.kind).collect();
        for k in crate::info().object_kinds.iter().map(|k| k.id).filter(|k| *k != kinds::TABLE) {
            assert!(kinds.contains(&k), "{k}");
        }
        // Each template splits into whole statements.
        let proc = &create_templates()[1].template;
        assert_eq!(crate::script::split(proc).len(), 1);
        assert_eq!(crate::script::split(&create_templates()[3].template).len(), 2);
    }
    #[test]
    fn sync_alters_columns_and_finds_generated_names() {
        use dbine_driver::alter::TableChange;
        let mut old = table();
        old.primary_key.as_mut().unwrap().name = None;
        old.foreign_keys[0].name = None;
        let mut new = table();
        new.primary_key.as_mut().unwrap().columns.push("ESTADO".into());
        new.foreign_keys.clear();
        new.columns[2].data_type = "VARCHAR(40)".into();
        new.columns[1].nullable = false;
        new.columns[4].data_type = "COMPUTED BY (ID * 3)".into();
        new.columns.retain(|c| c.name != "NOTA");
        new.columns.push(ColumnDef { name: "EMAIL".into(), data_type: "VARCHAR(100)".into(), nullable: true, ..Default::default() });
        new.indexes.retain(|i| i.name == "IX_UP");
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        let st: Vec<&str> = s.statements.iter().map(String::as_str).collect();
        assert!(st[0].starts_with("EXECUTE BLOCK AS\n  DECLARE n VARCHAR(255);\nBEGIN\n  FOR SELECT TRIM(c.RDB$CONSTRAINT_NAME)"), "{st:?}");
        assert!(st[0].contains("s.RDB$FIELD_NAME = 'CLIENTE_ID' AND u.RDB$RELATION_NAME = 'CLIENTES' INTO :n DO\n    EXECUTE STATEMENT 'ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"' || n || '\"';"), "{}", st[0]);
        assert_eq!(st[1], "DROP INDEX \"IX_ESTADO\";");
        assert!(st[2].contains("WHERE RDB$CONSTRAINT_NAME = 'UQ_NOTA')) THEN\n    EXECUTE STATEMENT 'ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"UQ_NOTA\"';\n  ELSE\n    EXECUTE STATEMENT 'DROP INDEX \"UQ_NOTA\"';"), "{}", st[2]);
        assert!(st[3].contains("RDB$RELATION_NAME = 'PEDIDOS' AND RDB$CONSTRAINT_TYPE = 'PRIMARY KEY' INTO :n DO"), "{}", st[3]);
        assert_eq!(
            &st[4..],
            [
                "ALTER TABLE \"PEDIDOS\" DROP \"NOTA\";",
                "ALTER TABLE \"PEDIDOS\" ADD \"EMAIL\" VARCHAR(100);",
                "ALTER TABLE \"PEDIDOS\" ALTER COLUMN \"CLIENTE_ID\" SET NOT NULL;",
                "ALTER TABLE \"PEDIDOS\" ALTER COLUMN \"ESTADO\" TYPE VARCHAR(40);",
                "ALTER TABLE \"PEDIDOS\" ALTER COLUMN \"DOBLE\" COMPUTED BY (ID * 3);",
                "ALTER TABLE \"PEDIDOS\" ADD CONSTRAINT \"PK_PEDIDOS\" PRIMARY KEY (\"ID\", \"ESTADO\");",
            ]
        );
        // Every block is one statement for the splitter.
        assert_eq!(crate::script::split(&s.statements.join("\n")).len(), s.statements.len());
    }

    #[test]
    fn checks_and_inactive_indexes() {
        use dbine_driver::alter::TableChange;
        assert_eq!(check_condition("CHECK (N > 0)").as_deref(), Some("(N > 0)"));
        assert_eq!(check_condition("check(N>0)").as_deref(), Some("(N>0)"));
        let mut old = table();
        old.checks = vec![
            CheckDef { name: None, expression: "(ID > 0)".into() },
            CheckDef { name: None, expression: "(ESTADO <> 'x')".into() },
            CheckDef { name: Some("CK_N".into()), expression: "(ID < 10)".into() },
        ];
        old.indexes[0].options.insert(INACTIVE.into(), "true".into());
        let ddl = table_ddl(&old, DdlParts { create: true, indexes: true, ..Default::default() });
        assert!(ddl.contains("    CHECK (ID > 0),\n") && ddl.contains("    CONSTRAINT \"CK_N\" CHECK (ID < 10)\n);"), "{ddl}");
        assert!(ddl.contains(&format!("ALTER INDEX {} INACTIVE;", q(&old.indexes[0].name))), "{ddl}");
        let mut new = old.clone();
        new.checks = vec![CheckDef { name: None, expression: "ID > 0".into() }, CheckDef { name: Some("CK_N".into()), expression: "(ID < 20)".into() }];
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        let st: Vec<&str> = s.statements.iter().map(String::as_str).collect();
        // The unnamed one that goes is found by its source; the other stays.
        assert_eq!(st.iter().filter(|s| s.contains("TRIM(SUBSTRING(tr.RDB$TRIGGER_SOURCE FROM 6)) = '(ESTADO <> ''x'')'")).count(), 1, "{st:#?}");
        assert!(!st.iter().any(|s| s.contains("(ID > 0)")), "{st:#?}");
        assert!(st.contains(&"ALTER TABLE \"PEDIDOS\" DROP CONSTRAINT \"CK_N\";"), "{st:#?}");
        assert!(st.contains(&"ALTER TABLE \"PEDIDOS\" ADD CONSTRAINT \"CK_N\" CHECK (ID < 20);"), "{st:#?}");
        assert_eq!(crate::script::split(&s.statements.join("\n")).len(), s.statements.len());
    }

    #[test]
    fn sync_creates_and_drops_without_schemas() {
        use dbine_driver::alter::TableChange;
        let mut t = table();
        t.schema = Some("X".into());
        let s = sync_script(&[TableChange::Drop { table: TableSchema { name: "VIEJA".into(), ..Default::default() } }, TableChange::Create { table: t }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE \"VIEJA\";");
        assert!(s.statements[1].starts_with("CREATE TABLE \"PEDIDOS\" ("), "{}", s.statements[1]);
        assert!(s.statements.last().unwrap().starts_with("ALTER TABLE \"PEDIDOS\" ADD CONSTRAINT \"FK_CLIENTE\""));
    }
}
