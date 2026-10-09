//! "Renombrar…" per variant: what each engine renames and the statements.
//!
//! The MySQL servers (and TiDB) rename tables and views with `RENAME
//! TABLE`, columns with `CHANGE COLUMN` and the whole column (it works on
//! every version, unlike `RENAME COLUMN`), and indexes with `RENAME INDEX`.
//! The engines that only emulate MySQL rename tables with their own
//! `ALTER TABLE` and nothing else. None of them renames constraints, and
//! their DDL commits by itself. MySQL and MariaDB rename databases by moving
//! what they hold (`rename_db`).
//!
//! MySQL refuses to rename a column a `CHECK` uses, and TiDB drops the
//! `CHECK` silently: the checks on the column are dropped and added back
//! with the new name around the `CHANGE COLUMN`. MariaDB updates them.

use crate::design::column_definition;
use crate::Variant;
use dbine_driver::rename::{quote_new, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::sql::{code_tokens, quote_ident, Quote, TokenKind};
use dbine_driver::{kinds, CheckDef, ColumnDef, Error, ObjectRef, Result, SyncScript, TableSchema};

pub(crate) const NO_DDL_TRANSACTIONS: &str =
    "El DDL de MySQL no es transaccional: cada sentencia se confirma sola y, si una falla, las anteriores quedan hechas.";
pub(crate) const NOTE_MYSQL: &str = "Las vistas que se reescriben se reponen con CREATE OR REPLACE y conservan sus permisos; las rutinas y los triggers se borran y se vuelven a crear (MySQL no tiene CREATE OR REPLACE para ellos), así que pierden los permisos otorgados sobre ellos. Para crear uno cuyo DEFINER no es tu usuario hace falta el privilegio SET_USER_ID (SET_ANY_DEFINER desde MySQL 8.2) o SUPER.";
pub(crate) const NOTE_MARIADB: &str = "Las vistas, rutinas y triggers que se reescriben se vuelven a crear con CREATE OR REPLACE (en las rutinas y los triggers equivale a borrarlos y crearlos). Para crear uno cuyo DEFINER no es tu usuario hace falta el privilegio SET USER o SUPER.";
pub(crate) const NOTE_TIDB: &str = "Las vistas que se reescriben se vuelven a crear con CREATE OR REPLACE.";
pub(crate) const NOTE_EMULATED: &str = "Este motor solo renombra tablas desde DBine. Las vistas que se reescriben se borran y se vuelven a crear.";
pub(crate) const INDEX_VERSION: &str = "RENAME INDEX necesita MySQL 5.7 o MariaDB 10.5 en adelante; en versiones anteriores el servidor rechaza la sentencia.";

/// What the variant renames; `None`: nothing.
pub(crate) fn spec(v: Variant) -> Option<RenameSpec> {
    let base = |kinds: &[&str], columns: bool, indexes: bool, replace: ReplaceStyle, note: &str| RenameSpec {
        kinds: kinds.iter().map(|k| k.to_string()).collect(),
        columns,
        indexes,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        transactional: false,
        note: Some(format!("{NO_DDL_TRANSACTIONS} {note}")),
        ..Default::default()
    };
    let relations = [kinds::TABLE, kinds::VIEW];
    let spec = match v.base() {
        // Views with CREATE OR REPLACE (they keep their grants); routines
        // and triggers have none, so they're dropped and created.
        Variant::MySql => RenameSpec {
            replace_kinds: [(kinds::VIEW.to_string(), ReplaceStyle::CreateOrReplace)].into(),
            ..base(&relations, true, true, ReplaceStyle::DropCreate, NOTE_MYSQL)
        },
        Variant::MariaDb => base(&relations, true, true, ReplaceStyle::CreateOrReplace, NOTE_MARIADB),
        // No routines or triggers: only views to put back.
        Variant::TiDb => base(&relations, true, true, ReplaceStyle::CreateOrReplace, NOTE_TIDB),
        Variant::OceanBase | Variant::SingleStore | Variant::StarRocks | Variant::Doris | Variant::Databend | Variant::GreptimeDb => {
            base(&[kinds::TABLE], false, false, ReplaceStyle::DropCreate, NOTE_EMULATED)
        }
        Variant::Manticore => return None,
        _ => unreachable!("managed variants map to their base"),
    };
    Some(if crate::rename_db::supported(v) {
        RenameSpec { databases: true, database_moves: true, database_note: Some(crate::rename_db::NOTE.into()), ..spec }
    } else {
        spec
    })
}

fn q(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

/// `db`.`name`, or the bare name without a database.
fn qualified(db: Option<&str>, name: &str) -> String {
    match db {
        Some(d) => format!("{}.{name}", q(d)),
        None => name.to_string(),
    }
}

fn written(v: Variant, name: &str) -> String {
    quote_new(name, &crate::script_dialect(v), Fold::None, false)
}

/// The statements that rename the target on `v`.
pub(crate) fn script(v: Variant, req: &RenameRequest) -> Result<SyncScript> {
    let Some(spec) = spec(v) else {
        return Err(Error::Unsupported("este motor no renombra objetos".into()));
    };
    let new = written(v, &req.new_name);
    match &req.target {
        RenameTarget::Object { object, .. } if spec.kinds.contains(&object.kind) => Ok(SyncScript { statements: vec![object_sql(v, object, &new)], warnings: vec![] }),
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW => Err(Error::Unsupported("este motor no renombra vistas: creá la vista con el nombre nuevo y borrá la anterior".into())),
        RenameTarget::Object { .. } => Err(Error::Unsupported("MySQL solo renombra tablas y vistas; las rutinas y los triggers se crean con el nombre nuevo".into())),
        RenameTarget::Column { table, column } if spec.columns => column_script(v, table, column, &req.new_name, req.table.as_ref()),
        RenameTarget::Index { table, index } if spec.indexes => {
            if index.eq_ignore_ascii_case("PRIMARY") {
                return Err(Error::Unsupported("la clave primaria de MySQL se llama siempre PRIMARY y no se renombra".into()));
            }
            let statement = format!("ALTER TABLE {} RENAME INDEX {} TO {new};", qualified(table.schema(), &q(&table.name)), q(index));
            let warnings = if matches!(v, Variant::MySql | Variant::MariaDb) { vec![INDEX_VERSION.to_string()] } else { vec![] };
            Ok(SyncScript { statements: vec![statement], warnings })
        }
        RenameTarget::Column { .. } => Err(Error::Unsupported("este motor no renombra columnas desde DBine".into())),
        RenameTarget::Index { .. } => Err(Error::Unsupported("este motor no renombra índices desde DBine".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("MySQL no renombra restricciones: hay que borrarlas y crearlas con el nombre nuevo".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("MySQL no renombra bases de datos".into())),
    }
}

/// A table or view, in the engine's syntax; it stays in its database.
fn object_sql(v: Variant, object: &ObjectRef, new: &str) -> String {
    let db = object.schema();
    let old = qualified(db, &q(&object.name));
    let to = qualified(db, new);
    match v.base() {
        Variant::StarRocks | Variant::Doris | Variant::GreptimeDb => format!("ALTER TABLE {old} RENAME {new};"),
        Variant::SingleStore => format!("ALTER TABLE {old} RENAME TO {new};"),
        _ => format!("RENAME TABLE {old} TO {to};"),
    }
}

/// `CHANGE COLUMN` with the column as the catalog describes it, and the
/// checks that use it dropped first and added back after where the engine
/// doesn't follow them.
fn column_script(v: Variant, table: &ObjectRef, column: &str, new_name: &str, schema: Option<&TableSchema>) -> Result<SyncScript> {
    let t = schema.ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la tabla «{}» para renombrar la columna", table.name)))?;
    let c = t
        .columns
        .iter()
        .find(|c| c.name == column)
        .or_else(|| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(column)))
        .ok_or_else(|| Error::Query(format!("la tabla «{}» no tiene la columna «{column}»", table.name)))?;
    let mut renamed: TableSchema = t.clone();
    for x in &mut renamed.columns {
        if x.name == c.name {
            x.name = new_name.to_string();
            // A MariaDB column CHECK names the column.
            x.data_type = rename_column_in(v, &x.data_type, &c.name, new_name);
        }
    }
    if let Some(pk) = &mut renamed.primary_key {
        for p in &mut pk.columns {
            if *p == c.name {
                *p = new_name.to_string();
            }
        }
    }
    let new_col = ColumnDef { name: new_name.to_string(), ..c.clone() };
    let def = column_definition(v, &renamed, &new_col);
    // `column_definition` writes the name in backticks; the new one goes as
    // the rewrite of the dependents writes it.
    let def = format!("{}{}", written(v, new_name), &def[q(new_name).len()..]);
    let name = qualified(table.schema(), &q(&table.name));
    let change = format!("CHANGE COLUMN {} {def}", q(&c.name));

    let checks: Vec<&CheckDef> = if v.base() == Variant::MariaDb { vec![] } else { t.checks.iter().filter(|k| mentions(v, &k.expression, &c.name)).collect() };
    let mut statements = Vec::new();
    let mut warnings = Vec::new();
    let mut unnamed = false;
    let readd: Vec<String> = checks
        .iter()
        .filter_map(|k| {
            let Some(n) = k.name.as_deref() else {
                unnamed = true;
                return None;
            };
            Some(format!("CONSTRAINT {} CHECK ({})", q(n), rename_column_in(v, strip_parens(&k.expression), &c.name, new_name)))
        })
        .collect();
    let drops: Vec<String> = checks.iter().filter_map(|k| k.name.as_deref()).map(|n| format!("DROP CHECK {}", q(n))).collect();
    if unnamed {
        warnings.push("Hay un CHECK sin nombre sobre la columna: revisalo después de renombrar.".to_string());
    }
    if drops.is_empty() {
        statements.push(format!("ALTER TABLE {name} {change};"));
    } else if v.base() == Variant::MySql {
        // One statement: MySQL applies it whole or not at all.
        let parts: Vec<String> = drops.into_iter().chain([change]).chain(readd.into_iter().map(|a| format!("ADD {a}"))).collect();
        statements.push(format!("ALTER TABLE {name} {};", parts.join(", ")));
    } else {
        // TiDB doesn't take a check drop with other changes.
        statements.extend(drops.into_iter().map(|d| format!("ALTER TABLE {name} {d};")));
        statements.push(format!("ALTER TABLE {name} {change};"));
        statements.extend(readd.into_iter().map(|a| format!("ALTER TABLE {name} ADD {a};")));
    }
    if text_type(&c.data_type) {
        warnings.push(format!(
            "CHANGE COLUMN vuelve a escribir la columna «{}» entera: si tiene un juego de caracteres o una intercalación distintos de los de la tabla, agregalos a la sentencia antes de ejecutarla.",
            c.name
        ));
    }
    Ok(SyncScript { statements, warnings })
}

/// A string column: its charset and collation aren't in the catalog copy.
fn text_type(ty: &str) -> bool {
    let t = ty.trim_start().to_ascii_lowercase();
    ["char", "varchar", "tinytext", "text", "mediumtext", "longtext", "enum", "set"].iter().any(|k| t.starts_with(k) && t[k.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_'))
}

/// `(expr)` as MySQL reports a CHECK, without the outer parentheses.
fn strip_parens(e: &str) -> &str {
    let t = e.trim();
    if t.starts_with('(') && t.ends_with(')') && crate::structure::inside(t, 0).is_some_and(|b| b.len() + 2 == t.len()) {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

fn mentions(v: Variant, expr: &str, column: &str) -> bool {
    code_tokens(expr, &crate::script_dialect(v)).iter().any(|t| t.kind == TokenKind::Name && t.text.eq_ignore_ascii_case(column))
}

/// `text` with every name token `old` written as the new name.
fn rename_column_in(v: Variant, text: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for t in code_tokens(text, &crate::script_dialect(v)) {
        if t.kind == TokenKind::Name && t.text.eq_ignore_ascii_case(old) {
            out.push_str(&text[at..t.start]);
            out.push_str(&q(new));
            at = t.end;
        }
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::KeyDef;

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some("shop".into()), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn object(kind: &str, name: &str) -> RenameTarget {
        RenameTarget::Object { object: obj(kind, name), parent: None }
    }

    fn table() -> TableSchema {
        TableSchema {
            name: "clientes".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "int".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef {
                    name: "pepe".into(),
                    data_type: "varchar(20)".into(),
                    nullable: false,
                    default_value: Some("'x'".into()),
                    comment: Some("it's \\ here".into()),
                    ..Default::default()
                },
                ColumnDef { name: "total".into(), data_type: "decimal(10,2) GENERATED ALWAYS AS (`id` * 2) STORED".into(), nullable: true, ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            checks: vec![CheckDef { name: Some("ck_pepe".into()), expression: "(`pepe` <> _utf8mb4'')".into() }],
            ..Default::default()
        }
    }

    fn column(name: &str, new: &str) -> RenameRequest {
        RenameRequest { table: Some(table()), ..req(RenameTarget::Column { table: obj("table", "clientes"), column: name.into() }, new) }
    }

    #[test]
    fn specs_per_variant() {
        let my = spec(Variant::MySql).unwrap();
        assert_eq!(my.kinds, ["table", "view"]);
        assert!(my.columns && my.indexes && !my.constraints && !my.schemas && !my.transactional);
        assert_eq!(my.replace, ReplaceStyle::DropCreate);
        assert!(my.note.as_deref().unwrap().contains("SET_USER_ID"));
        assert_eq!(spec(Variant::AuroraMySql).unwrap().replace, ReplaceStyle::DropCreate);
        assert_eq!(spec(Variant::AuroraMySql).unwrap().replace_for(kinds::VIEW), ReplaceStyle::CreateOrReplace);
        assert_eq!(spec(Variant::MySql).unwrap().replace_for(kinds::TRIGGER), ReplaceStyle::DropCreate);
        assert_eq!(spec(Variant::MariaDb).unwrap().replace, ReplaceStyle::CreateOrReplace);
        assert_eq!(spec(Variant::TiDb).unwrap().replace, ReplaceStyle::CreateOrReplace);
        for v in [Variant::OceanBase, Variant::SingleStore, Variant::StarRocks, Variant::Doris, Variant::VeloDb, Variant::Databend, Variant::GreptimeDb] {
            let s = spec(v).unwrap();
            assert_eq!(s.kinds, ["table"], "{v:?}");
            assert!(!s.columns && !s.indexes, "{v:?}");
        }
        assert!(spec(Variant::Manticore).is_none());
        for v in [Variant::MySql, Variant::AuroraMySql, Variant::CloudSqlMySql, Variant::MariaDb] {
            let s = spec(v).unwrap();
            assert!(s.databases && s.database_moves && s.database_from.is_none(), "{v:?}");
            assert!(s.database_note.as_deref().unwrap().contains("No es atómico"), "{v:?}");
        }
        for v in [Variant::TiDb, Variant::OceanBase, Variant::SingleStore, Variant::StarRocks, Variant::Doris, Variant::Databend, Variant::GreptimeDb] {
            assert!(!spec(v).unwrap().databases, "{v:?}");
        }
    }

    #[test]
    fn tables_and_views() {
        let s = script(Variant::MySql, &req(object("table", "clientes"), "Clientes Viejos")).unwrap();
        assert_eq!(s.statements, ["RENAME TABLE `shop`.`clientes` TO `shop`.`Clientes Viejos`;"]);
        let s = script(Variant::MariaDb, &req(object("view", "v_cli"), "VCli")).unwrap();
        assert_eq!(s.statements, ["RENAME TABLE `shop`.`v_cli` TO `shop`.VCli;"]);
        let bare = RenameTarget::Object { object: ObjectRef { kind: "table".into(), schema: None, name: "a`b".into() }, parent: None };
        assert_eq!(script(Variant::TiDb, &req(bare, "c")).unwrap().statements, ["RENAME TABLE `a``b` TO c;"]);
        assert_eq!(script(Variant::Doris, &req(object("table", "t"), "u")).unwrap().statements, ["ALTER TABLE `shop`.`t` RENAME u;"]);
        assert_eq!(script(Variant::StarRocks, &req(object("table", "t"), "u")).unwrap().statements, ["ALTER TABLE `shop`.`t` RENAME u;"]);
        assert_eq!(script(Variant::SingleStore, &req(object("table", "t"), "u")).unwrap().statements, ["ALTER TABLE `shop`.`t` RENAME TO u;"]);
        assert_eq!(script(Variant::Databend, &req(object("table", "t"), "select")).unwrap().statements, ["RENAME TABLE `shop`.`t` TO `shop`.`select`;"]);
        assert!(matches!(script(Variant::Doris, &req(object("view", "v"), "w")), Err(Error::Unsupported(_))));
        assert!(matches!(script(Variant::MySql, &req(object("procedure", "p"), "q")), Err(Error::Unsupported(_))));
        assert!(matches!(script(Variant::Manticore, &req(object("table", "t"), "u")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn columns_keep_their_definition() {
        let s = script(Variant::MariaDb, &column("pepe", "Nuevo Nombre")).unwrap();
        // MariaDB follows its checks by itself.
        assert_eq!(s.statements, ["ALTER TABLE `shop`.`clientes` CHANGE COLUMN `pepe` `Nuevo Nombre` varchar(20) DEFAULT 'x' NOT NULL COMMENT 'it''s \\\\ here';"]);
        assert!(s.warnings[0].contains("intercalación"), "{:?}", s.warnings);
        // The primary key column keeps AUTO_INCREMENT and NOT NULL; no check uses it.
        let s = script(Variant::MySql, &column("id", "codigo")).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `shop`.`clientes` CHANGE COLUMN `id` codigo int AUTO_INCREMENT NOT NULL;"]);
        assert!(s.warnings.is_empty());
        let s = script(Variant::MySql, &column("total", "doble")).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `shop`.`clientes` CHANGE COLUMN `total` doble decimal(10,2) GENERATED ALWAYS AS (`id` * 2) STORED NULL;"]);
    }

    #[test]
    fn mysql_checks_go_and_come_back() {
        let s = script(Variant::MySql, &column("pepe", "nuevo")).unwrap();
        assert_eq!(
            s.statements,
            ["ALTER TABLE `shop`.`clientes` DROP CHECK `ck_pepe`, CHANGE COLUMN `pepe` nuevo varchar(20) DEFAULT 'x' NOT NULL COMMENT 'it''s \\\\ here', ADD CONSTRAINT `ck_pepe` CHECK (`nuevo` <> _utf8mb4'');"]
        );
        let s = script(Variant::TiDb, &column("pepe", "nuevo")).unwrap();
        assert_eq!(s.statements.len(), 3);
        assert_eq!(s.statements[0], "ALTER TABLE `shop`.`clientes` DROP CHECK `ck_pepe`;");
        assert!(s.statements[1].starts_with("ALTER TABLE `shop`.`clientes` CHANGE COLUMN `pepe` nuevo varchar(20)"));
        assert_eq!(s.statements[2], "ALTER TABLE `shop`.`clientes` ADD CONSTRAINT `ck_pepe` CHECK (`nuevo` <> _utf8mb4'');");
    }

    #[test]
    fn mariadb_column_check_follows_the_name() {
        let mut r = column("pepe", "nuevo");
        r.table.as_mut().unwrap().columns[1].data_type = "varchar(20) CHECK (`pepe` <> '')".into();
        let s = script(Variant::MariaDb, &r).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `shop`.`clientes` CHANGE COLUMN `pepe` nuevo varchar(20) DEFAULT 'x' NOT NULL COMMENT 'it''s \\\\ here' CHECK (`nuevo` <> '');"]);
    }

    #[test]
    fn column_needs_the_table() {
        let mut r = column("pepe", "nuevo");
        r.table = None;
        assert!(matches!(script(Variant::MySql, &r), Err(Error::Query(_))));
        assert!(matches!(script(Variant::MySql, &column("nada", "x")), Err(Error::Query(_))));
        assert!(matches!(script(Variant::Doris, &column("pepe", "x")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn indexes() {
        let ix = |name: &str| RenameTarget::Index { table: obj("table", "clientes"), index: name.into() };
        let s = script(Variant::MySql, &req(ix("ix_pepe"), "IX Nuevo")).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `shop`.`clientes` RENAME INDEX `ix_pepe` TO `IX Nuevo`;"]);
        assert_eq!(s.warnings, [INDEX_VERSION]);
        let s = script(Variant::TiDb, &req(ix("ix_pepe"), "ix_nuevo")).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `shop`.`clientes` RENAME INDEX `ix_pepe` TO ix_nuevo;"]);
        assert!(s.warnings.is_empty());
        assert!(matches!(script(Variant::MySql, &req(ix("PRIMARY"), "pk")), Err(Error::Unsupported(_))));
        assert!(matches!(script(Variant::OceanBase, &req(ix("ix"), "iy")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn databases_and_constraints_are_refused() {
        let c = RenameTarget::Constraint { table: obj("table", "clientes"), constraint: "fk".into() };
        assert!(matches!(script(Variant::MySql, &req(c, "fk2")), Err(Error::Unsupported(_))));
        let d = RenameTarget::Schema { database: None, schema: "shop".into() };
        assert!(matches!(script(Variant::MariaDb, &req(d, "tienda")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn text_types() {
        assert!(text_type("varchar(20)") && text_type("enum('a','b')") && text_type("text") && text_type("char(2)"));
        assert!(!text_type("int") && !text_type("datetime") && !text_type("character_x") && !text_type("setx"));
    }
}
