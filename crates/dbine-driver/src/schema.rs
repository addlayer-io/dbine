//! Structure of a database beyond the object list: tables with their keys,
//! foreign keys and indexes (for the ER diagram and the script generator),
//! and what the table designer offers per engine.
//!
//! [`TableSchema`] is both what an engine reports and what the designer
//! edits: the designer builds one and the driver turns it into DDL in its
//! own language ([`crate::Driver::table_ddl`]).

use crate::info::Field;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TableSchema {
    /// Object kind (`table`, `collection`, `index`…).
    #[serde(default = "table_kind")]
    pub kind: String,
    #[serde(default)]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default)]
    pub columns: Vec<ColumnDef>,
    #[serde(default)]
    pub primary_key: Option<KeyDef>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// Indexes and unique constraints (not the primary key).
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
    /// `CHECK` constraints.
    #[serde(default)]
    pub checks: Vec<CheckDef>,
    #[serde(default)]
    pub comment: Option<String>,
    /// Engine-specific table options, keyed like the designer's
    /// `table_options` fields (engine, partitioning, TTL, shards…).
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// What the reader couldn't read and left out (a CHECK, a default or a
    /// computed expression the login can't see), and why (Spanish, for the
    /// user): a migration or script built from this table shows it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl TableSchema {
    /// Leaves out the CHECKs, defaults and computed expressions that came
    /// empty (a definition the reader couldn't see), so no DDL is written as
    /// `CHECK ()`, `DEFAULT` or `AS` with nothing after it. Returns what it
    /// left out (Spanish, for the user); the table's own `warnings` aren't
    /// included.
    pub fn drop_empty_definitions(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let table = self.name.clone();
        self.checks.retain(|c| {
            let empty = check_body_is_empty(&c.expression);
            if empty {
                let name = c.name.clone().unwrap_or_default();
                out.push(format!(
                    "{table}: la restricción CHECK {name} vino sin definición (no se pudo leer) y no se crea"
                ));
            }
            !empty
        });
        self.columns.retain_mut(|c| {
            if c.default_value
                .as_deref()
                .is_some_and(check_body_is_empty)
            {
                c.default_value = None;
                out.push(format!(
                    "{table}: el valor por defecto de la columna {} vino sin definición (no se pudo leer); la columna queda sin él",
                    c.name
                ));
            }
            if computed_is_empty(&c.data_type) {
                out.push(format!(
                    "{table}: la columna calculada {} vino sin expresión (no se pudo leer) y no se crea",
                    c.name
                ));
                return false;
            }
            true
        });
        out
    }
}

/// Nothing but blanks and parentheses: `""`, `"()"`, `"( )"`.
fn check_body_is_empty(expr: &str) -> bool {
    !crate::ddl::has_body(expr)
}

/// A computed column's type with no expression: `AS`, `AS PERSISTED`,
/// `AS ()`, `GENERATED ALWAYS AS ()`, `… AS () STORED`.
fn computed_is_empty(data_type: &str) -> bool {
    let u = data_type.trim().to_ascii_uppercase();
    let words: Vec<&str> = u.split_whitespace().collect();
    if words.first() == Some(&"AS") {
        return words[1..]
            .iter()
            .all(|w| check_body_is_empty(w) || *w == "PERSISTED");
    }
    match u
        .find(" AS ")
        .or_else(|| u.ends_with(" AS").then(|| u.len() - 3))
    {
        Some(i) if u[..i].trim_end().ends_with("ALWAYS") => {
            let rest = u[i + 3..].trim();
            let body = rest
                .trim_end_matches("STORED")
                .trim_end_matches("VIRTUAL")
                .trim_end_matches("PERSISTED");
            check_body_is_empty(body)
        }
        _ => false,
    }
}

fn table_kind() -> String {
    crate::kinds::TABLE.to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    /// Type as the engine spells it (`nvarchar(50)`, `jsonb`, `text`…).
    pub data_type: String,
    #[serde(default = "yes")]
    pub nullable: bool,
    /// SQL expression / literal as the engine takes it.
    #[serde(default)]
    pub default_value: Option<String>,
    #[serde(default)]
    pub auto_increment: bool,
    #[serde(default)]
    pub comment: Option<String>,
    /// Engine-specific column options, keyed like the designer's
    /// `column_options` fields (Cassandra partition/clustering key…).
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KeyDef {
    #[serde(default)]
    pub name: Option<String>,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ForeignKeyDef {
    #[serde(default)]
    pub name: Option<String>,
    pub columns: Vec<String>,
    #[serde(default)]
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    /// `CASCADE`, `SET NULL`… (`None` = the engine's default).
    #[serde(default)]
    pub on_delete: Option<String>,
    #[serde(default)]
    pub on_update: Option<String>,
}

/// A `CHECK` constraint.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CheckDef {
    #[serde(default)]
    pub name: Option<String>,
    /// The condition as the engine reports it (without `CHECK`).
    pub expression: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
    /// Access method / engine kind (`btree`, `gin`, `CLUSTERED`, `text`…).
    #[serde(default)]
    pub kind: Option<String>,
    /// Partial index predicate.
    #[serde(default)]
    pub filter: Option<String>,
    /// Non-key columns stored in the index (`INCLUDE (…)` in SQL Server,
    /// PostgreSQL, Db2…).
    #[serde(default)]
    pub include: Vec<String>,
    /// Engine-specific settings that make two indexes different (fill
    /// factor, compression, a full-text index's catalog and key index…),
    /// named as the engine names them.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
}

/// Which parts of a table's DDL to produce ([`crate::Driver::table_ddl`]).
/// Scripts emit every table's `create` before any `foreign_keys`, so
/// references never point at tables that don't exist yet.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DdlParts {
    /// `DROP` first.
    pub drop: bool,
    /// Guard DROP/CREATE with IF [NOT] EXISTS (or the engine's equivalent).
    pub if_exists: bool,
    /// The table itself: columns, primary key, comments, options.
    pub create: bool,
    pub indexes: bool,
    pub foreign_keys: bool,
}

/// What the table designer offers for an engine ([`crate::Driver::designer`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesignerSpec {
    /// Kind of object it creates (`table`, `collection`, `index`, `key`…).
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub kind: &'static str,
    /// Menu / title text: "Nueva tabla", "Nueva colección"…
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub label: &'static str,
    /// Suggestions for the type column (free text is allowed).
    #[serde(deserialize_with = "crate::serde_static::strs")]
    pub data_types: Vec<&'static str>,
    /// Schema selector (engines with schemas).
    pub schemas: bool,
    pub primary_key: bool,
    pub auto_increment: bool,
    pub defaults: bool,
    pub nullability: bool,
    pub comments: bool,
    pub indexes: bool,
    pub foreign_keys: bool,
    /// Extra per-column fields (stored in `ColumnDef::options`).
    pub column_options: Vec<Field>,
    /// Extra per-table fields (stored in `TableSchema::options`).
    pub table_options: Vec<Field>,
    /// Whether the designer needs columns at all (a Mongo collection or a
    /// Redis key may be created without them).
    pub columns_required: bool,
}

impl DesignerSpec {
    /// A relational table designer with every feature on; drivers switch
    /// off what their engine lacks.
    pub fn sql_table(data_types: Vec<&'static str>) -> Self {
        Self {
            kind: crate::kinds::TABLE,
            label: "Nueva tabla",
            data_types,
            schemas: false,
            primary_key: true,
            auto_increment: true,
            defaults: true,
            nullability: true,
            comments: false,
            indexes: true,
            foreign_keys: true,
            column_options: Vec::new(),
            table_options: Vec::new(),
            columns_required: true,
        }
    }
}

/// A starting script for creating an object that has no designer (views,
/// routines, triggers…), in the driver's language. The UI opens it in a
/// new query. `{schema}` and `{name}` are replaced by the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTemplate {
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub kind: &'static str,
    /// Menu text: "Nueva vista", "Nuevo procedimiento"…
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub label: &'static str,
    pub template: String,
}

/// Database-level operations a driver offers.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub create_database: bool,
    pub drop_database: bool,
    /// Reports foreign keys (relations for the ER diagram).
    pub foreign_keys: bool,
    /// Its sessions implement `Session::monitor` (the "Monitor" dashboard).
    pub monitor: bool,
    /// Its sessions implement `Session::blocking` (who blocks whom).
    #[serde(default)]
    pub blocking: bool,
    /// Its sessions implement `Session::kill_session`.
    #[serde(default)]
    pub kill_session: bool,
    /// Its sessions implement `Session::processes`.
    #[serde(default)]
    pub processes: bool,
    /// Its sessions implement `Session::cancel_query`.
    #[serde(default)]
    pub cancel_query: bool,
    /// Its sessions implement `Session::database_properties` and
    /// `alter_database` ("Propiedades" on a database).
    #[serde(default)]
    pub database_properties: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, data_type: &str, default: Option<&str>) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            data_type: data_type.into(),
            default_value: default.map(str::to_string),
            ..Default::default()
        }
    }

    fn check(name: &str, expression: &str) -> CheckDef {
        CheckDef {
            name: Some(name.into()),
            expression: expression.into(),
        }
    }

    #[test]
    fn empty_definitions_are_left_out_and_said() {
        let mut t = TableSchema {
            name: "t".into(),
            columns: vec![
                col("a", "int", Some("")),
                col("b", "AS  PERSISTED", None),
                col("c", "int GENERATED ALWAYS AS () STORED", None),
                col("d", "AS ([a]*(2))", Some("((0))")),
                col("e", "bigint GENERATED ALWAYS AS IDENTITY", None),
            ],
            checks: vec![
                check("CK_x", ""),
                check("CK_y", "( )"),
                check("CK_z", "([a]>(0))"),
            ],
            ..Default::default()
        };
        let notes = t.drop_empty_definitions();
        assert_eq!(notes.len(), 5, "{notes:?}");
        assert!(notes[0].contains("CK_x"));
        assert_eq!(t.checks, vec![check("CK_z", "([a]>(0))")]);
        let cols: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, ["a", "d", "e"]);
        assert_eq!(t.columns[0].default_value, None);
        assert_eq!(t.columns[1].default_value.as_deref(), Some("((0))"));
    }

    #[test]
    fn warnings_stay_out_of_the_json_when_empty() {
        let t = TableSchema {
            name: "t".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(&t).unwrap();
        assert!(v.get("warnings").is_none());
        let back: TableSchema = serde_json::from_value(v).unwrap();
        assert!(back.warnings.is_empty());
    }
}
