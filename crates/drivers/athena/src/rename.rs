//! "Renombrar…" on Athena, conservatively:
//!
//! - Tables: `ALTER TABLE … RENAME TO`, which Athena takes only for Iceberg
//!   tables (Glue keeps external Hive tables by name and Athena lists the
//!   statement as unsupported for them): the server refuses the others.
//! - Columns of Iceberg tables: `ALTER TABLE … CHANGE COLUMN old new type`
//!   (Iceberg tracks columns by id, so the data follows). External tables
//!   are refused here: the change is metadata only, and a Parquet or ORC
//!   file read by name would then read the column as NULL.
//! - Views: no `ALTER VIEW`, so the view is created with the new name and
//!   the old one dropped.
//!
//! Views bind names when queried: the app rewrites the ones that name the
//! target and puts them back with `CREATE OR REPLACE VIEW`. The DDL (Hive)
//! takes backticks; views run on Trino and take double quotes. Glue keeps
//! names in lower case, with letters, digits and underscores only, so the
//! new name is checked for that and never needs quotes in the DDL.

use crate::ddl::{column, q, TABLE_TYPE};
use dbine_driver::rename::{quote_new, rename_header, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::sql::{qualified_name, split_script, Quote, ScriptDialect};
use dbine_driver::{kinds, ColumnDef, Error, ObjectRef, Result, SyncScript};

pub const NOTE: &str = "Athena renombra tablas y columnas solo si son Iceberg: con una tabla externa (Hive) el servidor rechaza el cambio de nombre de la tabla, y DBine no renombra sus columnas porque en Parquet u ORC dejarían de leer sus datos. Las vistas se renombran creándolas con el nombre nuevo y borrando la anterior. Las vistas que usan el objeto no se actualizan solas: DBine las reescribe y las repone con CREATE OR REPLACE VIEW. Las sentencias no son transaccionales.";

pub const VIEW_RECREATED: &str = "La vista se crea con el nombre nuevo y se borra la anterior: se pierden los permisos de Lake Formation otorgados sobre ella.";

pub fn spec() -> Option<RenameSpec> {
    Some(RenameSpec {
        kinds: vec![kinds::TABLE.into(), kinds::VIEW.into()],
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: Fold::Lower,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    })
}

fn dialect() -> ScriptDialect {
    ScriptDialect::for_hint("trino")
}

/// `` `db`.`name` `` for the Hive DDL.
fn ddl_name(o: &ObjectRef) -> String {
    qualified_name(Quote::Backtick, o.schema().filter(|s| !s.is_empty()), &o.name)
}

pub fn script(req: &RenameRequest) -> Result<SyncScript> {
    let new = req.new_name.as_str();
    if new.is_empty() || !new.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Err(Error::Unsupported(format!(
            "Athena (el catálogo de Glue) solo acepta nombres en minúsculas, con letras, números y guiones bajos: probá con «{}»",
            suggestion(new)
        )));
    }
    match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE => {
            let to = ObjectRef { name: new.into(), ..object.clone() };
            Ok(SyncScript { statements: vec![format!("ALTER TABLE {} RENAME TO {};", ddl_name(object), ddl_name(&to))], warnings: vec![] })
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW => {
            let def = req.definition.as_deref().ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la vista «{}»", object.name)))?;
            let renamed = rename_header(def, &dialect(), Fold::Lower, new)
                .ok_or_else(|| Error::Query(format!("no se reconoce el encabezado de la definición de la vista «{}»", object.name)))?;
            let create = format!("{};", renamed.trim().trim_end_matches(';').trim_end());
            // Bare where Glue's names allow it: DROP VIEW reads them on either engine.
            let bare = |n: &str| quote_new(n, &dialect(), Fold::Lower, false);
            let old = match object.schema().filter(|s| !s.is_empty()) {
                Some(s) => format!("{}.{}", bare(s), bare(&object.name)),
                None => bare(&object.name),
            };
            let statements = vec![create, format!("DROP VIEW {old};")];
            one_unit_each(&statements, &format!("la definición de la vista «{}»", object.name))?;
            Ok(SyncScript { statements, warnings: vec![VIEW_RECREATED.into()] })
        }
        RenameTarget::Object { .. } => Err(Error::Unsupported("Athena solo renombra tablas, vistas y columnas".into())),
        RenameTarget::Column { table, column: name } => {
            let t = req.table.as_ref().ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la tabla «{}» para renombrar la columna", table.name)))?;
            let kind = t.options.get(TABLE_TYPE).map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty()).unwrap_or_else(|| "iceberg".into());
            if kind != "iceberg" {
                return Err(Error::Unsupported(format!(
                    "Athena solo renombra columnas de tablas Iceberg: «{}» es una tabla externa ({kind}) y en ella el cambio es solo de metadatos, así que en Parquet u ORC la columna dejaría de leer sus datos",
                    table.name
                )));
            }
            let c = t
                .columns
                .iter()
                .find(|c| c.name == *name)
                .or_else(|| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
                .ok_or_else(|| Error::Query(format!("la tabla «{}» no tiene la columna «{name}»", table.name)))?;
            // CHANGE COLUMN restates the type as Glue keeps it: only a type
            // the grammar below reads goes into the statement.
            if !valid_type(&c.data_type) {
                return Err(Error::Unsupported(format!(
                    "el tipo de la columna «{}» en el catálogo ({}) no es un tipo de Athena que DBine reconozca, así que no la renombra: hacelo a mano con ALTER TABLE … CHANGE COLUMN",
                    c.name,
                    printable(&c.data_type)
                )));
            }
            let renamed = column(&ColumnDef { name: new.into(), ..c.clone() });
            let statements = vec![format!("ALTER TABLE {} CHANGE COLUMN {} {renamed};", ddl_name(table), q(&c.name))];
            one_unit_each(&statements, &format!("el cambio de nombre de la columna «{}»", c.name))?;
            Ok(SyncScript { statements, warnings: vec![] })
        }
        RenameTarget::Index { .. } => Err(Error::Unsupported("Athena no tiene índices".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("Athena no tiene restricciones".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("Athena no renombra bases de datos".into())),
    }
}

/// Each statement must reach Athena whole: `execute` cuts what it gets with
/// the generic dialect, so a stored text whose `;` sits outside what that
/// dialect reads as a string or a comment would run what follows on its own.
fn one_unit_each(statements: &[String], what: &str) -> Result<()> {
    if statements.iter().any(|s| split_script(s, &ScriptDialect::generic()).len() > 1) {
        return Err(Error::Unsupported(format!(
            "{what} se partiría en varias sentencias al ejecutarlo, así que DBine no lo hace: renombrá a mano"
        )));
    }
    Ok(())
}

/// Glue's type, cut short and on one line, for an error message.
fn printable(ty: &str) -> String {
    let one_line: String = ty.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    match one_line.char_indices().nth(80) {
        Some((at, _)) => format!("{}…", &one_line[..at]),
        None => one_line,
    }
}

/// Athena's (Hive DDL) primitive type names.
const PRIMITIVES: &[&str] = &[
    "BOOLEAN", "TINYINT", "SMALLINT", "INT", "INTEGER", "BIGINT", "FLOAT", "REAL", "DOUBLE", "DECIMAL", "NUMERIC", "CHAR", "VARCHAR",
    "STRING", "BINARY", "DATE", "TIMESTAMP", "TIMESTAMPTZ", "TIME", "UUID",
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok<'a> {
    Word(&'a str),
    Num,
    Sym(u8),
}

/// A column type as Athena's DDL writes it: a primitive (`int`, `string`…),
/// `decimal(p[,s])`, `char(n)`, `varchar(n)`, or `array<t>`, `map<k,v>`,
/// `struct<name:t,…>` nested up to [`MAX_DEPTH`] levels. Only ASCII
/// letters, digits, `_`, spaces and `( ) , < > :`: no quotes, `;`,
/// comments, backticks or line breaks.
fn valid_type(ty: &str) -> bool {
    if ty.trim().is_empty() || !ty.bytes().all(|c| c.is_ascii_alphanumeric() || b"_ (),<>:".contains(&c)) {
        return false;
    }
    let b = ty.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' => i += 1,
            c if b"(),<>:".contains(&c) => {
                toks.push(Tok::Sym(c));
                i += 1;
            }
            c if c.is_ascii_digit() => {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
                    return false;
                }
                toks.push(Tok::Num);
            }
            _ => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                    i += 1;
                }
                toks.push(Tok::Word(&ty[s..i]));
            }
        }
    }
    let mut pos = 0;
    parse_type(&toks, &mut pos, 0) && pos == toks.len()
}

const MAX_DEPTH: usize = 8;

fn parse_type(toks: &[Tok], pos: &mut usize, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    let Some(Tok::Word(w)) = toks.get(*pos) else { return false };
    let name = w.to_ascii_uppercase();
    *pos += 1;
    let sym = |pos: &mut usize, c: u8| {
        let ok = toks.get(*pos) == Some(&Tok::Sym(c));
        if ok {
            *pos += 1;
        }
        ok
    };
    let num = |pos: &mut usize| {
        let ok = toks.get(*pos) == Some(&Tok::Num);
        if ok {
            *pos += 1;
        }
        ok
    };
    match name.as_str() {
        "ARRAY" => sym(pos, b'<') && parse_type(toks, pos, depth + 1) && sym(pos, b'>'),
        "MAP" => sym(pos, b'<') && parse_type(toks, pos, depth + 1) && sym(pos, b',') && parse_type(toks, pos, depth + 1) && sym(pos, b'>'),
        "STRUCT" => {
            if !sym(pos, b'<') {
                return false;
            }
            loop {
                // A field: `name:type`, the name a plain word.
                let Some(Tok::Word(_)) = toks.get(*pos) else { return false };
                *pos += 1;
                if !(sym(pos, b':') && parse_type(toks, pos, depth + 1)) {
                    return false;
                }
                if sym(pos, b'>') {
                    return true;
                }
                if !sym(pos, b',') {
                    return false;
                }
            }
        }
        "DECIMAL" | "NUMERIC" => !sym(pos, b'(') || (num(pos) && (!sym(pos, b',') || num(pos)) && sym(pos, b')')),
        "CHAR" | "VARCHAR" => !sym(pos, b'(') || (num(pos) && sym(pos, b')')),
        n => PRIMITIVES.contains(&n),
    }
}

/// The name as Glue would take it.
fn suggestion(name: &str) -> String {
    name.trim().to_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::TableSchema;

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: None, name: name.into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn table(kind: &str) -> TableSchema {
        let mut t = TableSchema {
            name: "ventas".into(),
            columns: vec![ColumnDef { name: "importe".into(), data_type: "decimal(10,2)".into(), comment: Some("El importe".into()), ..Default::default() }],
            ..Default::default()
        };
        t.options.insert(TABLE_TYPE.into(), kind.into());
        t
    }

    #[test]
    fn spec_is_tables_views_and_columns() {
        let s = spec().unwrap();
        assert_eq!(s.kinds, ["table", "view"]);
        assert!(s.columns && !s.indexes && !s.constraints && !s.schemas && !s.transactional);
        assert_eq!((s.replace, s.fold), (ReplaceStyle::CreateOrReplace, Fold::Lower));
    }

    #[test]
    fn tables() {
        assert_eq!(script(&req(RenameTarget::Object { object: obj("table", "ventas"), parent: None }, "ventas_2024")).unwrap().statements, ["ALTER TABLE `ventas` RENAME TO `ventas_2024`;"]);
        let qualified = ObjectRef { schema: Some("crudo".into()), ..obj("table", "ventas") };
        assert_eq!(script(&req(RenameTarget::Object { object: qualified, parent: None }, "v2")).unwrap().statements, ["ALTER TABLE `crudo`.`ventas` RENAME TO `crudo`.`v2`;"]);
    }

    #[test]
    fn names_glue_takes() {
        for bad in ["Ventas", "ventas netas", "ventas-2024", ""] {
            assert!(matches!(script(&req(RenameTarget::Object { object: obj("table", "t"), parent: None }, bad)), Err(Error::Unsupported(_))), "{bad}");
        }
        assert_eq!(suggestion("Ventas Netas-2024"), "ventas_netas_2024");
    }

    #[test]
    fn views_are_created_again() {
        let mut r = req(RenameTarget::Object { object: obj("view", "v"), parent: None }, "select");
        r.definition = Some("CREATE VIEW \"v\" AS\nSELECT importe FROM ventas".into());
        let s = script(&r).unwrap();
        assert_eq!(s.statements, ["CREATE VIEW \"select\" AS\nSELECT importe FROM ventas;", "DROP VIEW v;"]);
        assert_eq!(s.warnings, [VIEW_RECREATED]);
        r.definition = Some("CREATE VIEW crudo.v AS SELECT 1".into());
        r.new_name = "w".into();
        assert_eq!(script(&r).unwrap().statements[0], "CREATE VIEW crudo.w AS SELECT 1;");
        r.definition = None;
        assert!(matches!(script(&r), Err(Error::Query(_))));
    }

    #[test]
    fn columns_of_iceberg_tables_only() {
        let target = || RenameTarget::Column { table: obj("table", "ventas"), column: "importe".into() };
        let mut r = req(target(), "monto");
        assert!(matches!(script(&r), Err(Error::Query(_))));
        r.table = Some(table("iceberg"));
        assert_eq!(script(&r).unwrap().statements, ["ALTER TABLE `ventas` CHANGE COLUMN `importe` `monto` decimal(10,2) COMMENT 'El importe';"]);
        r.table = Some(table("parquet"));
        assert!(matches!(script(&r), Err(Error::Unsupported(_))));
    }

    #[test]
    fn column_types_glue_gives() {
        for ty in [
            "int",
            "BIGINT",
            "string",
            "decimal(10,2)",
            "decimal(38, 0)",
            "decimal",
            "varchar(255)",
            "char(3)",
            "timestamp",
            "array<string>",
            "map<string,array<int>>",
            "struct<a:int,b:struct<c:string,d:array<decimal(10,2)>>>",
            "array<struct<id:bigint,tags:map<string,string>>>",
        ] {
            assert!(valid_type(ty), "{ty}");
        }
    }

    #[test]
    fn column_types_refused() {
        let deep = format!("{}int{}", "array<".repeat(MAX_DEPTH + 1), ">".repeat(MAX_DEPTH + 1));
        assert!(valid_type(&format!("{}int{}", "array<".repeat(MAX_DEPTH), ">".repeat(MAX_DEPTH))));
        for ty in [
            "",
            "int; DROP TABLE finance.salaries; --",
            "int;",
            "int -- x",
            "int /* x */",
            "int\nDROP",
            "string COMMENT 'x'",
            "varchar(10",
            "decimal(10,2,3)",
            "array<int",
            "map<string>",
            "struct<a int>",
            "struct<`a`:int>",
            "struct<>",
            "array<foo>",
            "int int",
            "9int",
            "varchar(n)",
            &deep,
        ] {
            assert!(!valid_type(ty), "{ty:?}");
        }
    }

    #[test]
    fn a_column_with_a_type_outside_the_grammar_is_not_renamed() {
        let mut r = req(RenameTarget::Column { table: obj("table", "ventas"), column: "importe".into() }, "monto");
        let mut t = table("iceberg");
        t.columns[0].data_type = "int; DROP TABLE finance.salaries; --".into();
        r.table = Some(t);
        assert!(matches!(script(&r), Err(Error::Unsupported(m)) if m.contains("«importe»") && !m.contains('\n')));
        let mut t = table("iceberg");
        t.columns[0].data_type = "struct<a:int,b:array<string>>".into();
        r.table = Some(t);
        assert_eq!(script(&r).unwrap().statements, ["ALTER TABLE `ventas` CHANGE COLUMN `importe` `monto` struct<a:int,b:array<string>> COMMENT 'El importe';"]);
    }

    #[test]
    fn a_view_whose_text_would_split_is_not_renamed() {
        let mut r = req(RenameTarget::Object { object: obj("view", "v"), parent: None }, "w");
        r.definition = Some("CREATE VIEW v AS SELECT 1 AS x; DROP TABLE finance.salaries".into());
        assert!(matches!(script(&r), Err(Error::Unsupported(m)) if m.contains("«v»")));
        r.definition = Some("CREATE VIEW v AS SELECT 'a;b' AS x -- c;d\nFROM t".into());
        assert_eq!(script(&r).unwrap().statements, ["CREATE VIEW w AS SELECT 'a;b' AS x -- c;d\nFROM t;", "DROP VIEW v;"]);
    }

    #[test]
    fn refused() {
        for t in [
            RenameTarget::Object { object: obj("function", "f"), parent: None },
            RenameTarget::Index { table: obj("table", "t"), index: "i".into() },
            RenameTarget::Constraint { table: obj("table", "t"), constraint: "c".into() },
            RenameTarget::Schema { database: None, schema: "crudo".into() },
        ] {
            assert!(matches!(script(&req(t, "x")), Err(Error::Unsupported(_))));
        }
    }
}
