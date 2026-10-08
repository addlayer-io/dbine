//! The database's structure as the rules ask it: a table by the name the
//! query wrote, a column's type and nullability, its keys and indexes.

use dbine_driver::{ColumnDef, TableSchema};

pub struct Catalog<'a> {
    pub tables: &'a [TableSchema],
    /// The schema an unqualified name means when several match (`public`, `dbo`).
    pub default_schema: Option<&'a str>,
}

impl<'a> Catalog<'a> {
    pub fn new(tables: &'a [TableSchema], dialect: &str) -> Self {
        let default_schema = match dialect {
            "postgres" => Some("public"),
            "mssql" | "sybase" => Some("dbo"),
            _ => None,
        };
        Self { tables, default_schema }
    }

    /// The table `parts` names (`[schema.]table`, the database part ignored).
    pub fn table(&self, parts: &[String]) -> Option<&'a TableSchema> {
        let name = parts.last()?;
        let schema = (parts.len() >= 2).then(|| parts[parts.len() - 2].as_str());
        let matches: Vec<&TableSchema> = self
            .tables
            .iter()
            .filter(|t| t.name.eq_ignore_ascii_case(name))
            .filter(|t| schema.is_none_or(|s| t.schema.as_deref().is_some_and(|ts| ts.eq_ignore_ascii_case(s))))
            .collect();
        match matches.len() {
            0 => None,
            1 => Some(matches[0]),
            _ => {
                let exact: Vec<_> = matches.iter().filter(|t| t.name == *name).collect();
                if exact.len() == 1 {
                    return Some(exact[0]);
                }
                let d = self.default_schema?;
                matches.into_iter().find(|t| t.schema.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(d)))
            }
        }
    }

    pub fn column<'t>(t: &'t TableSchema, name: &str) -> Option<&'t ColumnDef> {
        t.columns.iter().find(|c| c.name == name).or_else(|| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
    }

    /// The column can't hold NULL (NOT NULL, or part of the primary key).
    pub fn not_null(t: &TableSchema, name: &str) -> bool {
        let in_pk = t.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(|c| c.eq_ignore_ascii_case(name)));
        in_pk || Self::column(t, name).is_some_and(|c| !c.nullable)
    }

    /// Column sets that identify a row: the primary key, and unique indexes
    /// without a filter whose columns are all NOT NULL.
    pub fn keys(t: &TableSchema) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        if let Some(pk) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
            out.push(pk.columns.clone());
        }
        for ix in &t.indexes {
            if ix.unique && ix.filter.as_deref().is_none_or(str::is_empty) && !ix.columns.is_empty() && ix.columns.iter().all(|c| Self::not_null(t, c)) {
                out.push(ix.columns.clone());
            }
        }
        out
    }

    /// Indexes as column lists, the primary key first.
    pub fn index_columns(t: &TableSchema) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        if let Some(pk) = t.primary_key.as_ref().filter(|k| !k.columns.is_empty()) {
            out.push(pk.columns.clone());
        }
        out.extend(t.indexes.iter().filter(|i| !i.columns.is_empty()).map(|i| i.columns.iter().map(|c| index_column(c)).collect()));
        out
    }

    /// Some index (or the primary key) starts with this column.
    pub fn leads_an_index(t: &TableSchema, column: &str) -> bool {
        Self::index_columns(t).iter().any(|cols| cols.first().is_some_and(|c| c.eq_ignore_ascii_case(column)))
    }
}

/// An index column as reported, without its direction (`fecha DESC`).
pub fn index_column(c: &str) -> String {
    let t = c.trim();
    for suffix in [" DESC", " ASC", " desc", " asc"] {
        if let Some(s) = t.strip_suffix(suffix) {
            return super::lex::unquote(s.trim());
        }
    }
    super::lex::unquote(t)
}

/// What kind of values a type holds, from its name as the engine spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeClass {
    Text,
    Number,
    /// A date, or a date with a time.
    DateTime,
    Other,
}

pub fn type_class(data_type: &str) -> TypeClass {
    let t = data_type.to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    const TEXT: &[&str] = &["char", "varchar", "nchar", "nvarchar", "text", "ntext", "tinytext", "mediumtext", "longtext", "string", "varchar2", "nvarchar2", "clob", "nclob", "character", "citext", "bpchar", "sysname"];
    const NUMBER: &[&str] = &[
        "int", "integer", "smallint", "tinyint", "bigint", "mediumint", "decimal", "numeric", "number", "float", "real", "double", "money", "smallmoney",
        "int2", "int4", "int8", "float4", "float8", "serial", "bigserial", "smallserial", "binary_float", "binary_double",
    ];
    const DATES: &[&str] = &["date", "datetime", "datetime2", "smalldatetime", "timestamp", "timestamptz"];
    if TEXT.contains(&base) || t.starts_with("character varying") {
        TypeClass::Text
    } else if NUMBER.contains(&base) || t.starts_with("double precision") {
        TypeClass::Number
    } else if DATES.contains(&base) {
        TypeClass::DateTime
    } else {
        TypeClass::Other
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dbine_driver::{IndexDef, KeyDef};

    pub fn table(schema: Option<&str>, name: &str, cols: &[(&str, &str, bool)], pk: &[&str], indexes: &[(&[&str], bool)]) -> TableSchema {
        TableSchema {
            schema: schema.map(str::to_string),
            name: name.into(),
            columns: cols.iter().map(|(n, t, null)| ColumnDef { name: n.to_string(), data_type: t.to_string(), nullable: *null, ..Default::default() }).collect(),
            primary_key: (!pk.is_empty()).then(|| KeyDef { name: None, columns: pk.iter().map(|c| c.to_string()).collect() }),
            indexes: indexes
                .iter()
                .enumerate()
                .map(|(i, (c, u))| IndexDef { name: format!("ix{i}"), columns: c.iter().map(|c| c.to_string()).collect(), unique: *u, ..Default::default() })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn tables_by_name_and_schema() {
        let ts = vec![table(Some("public"), "t", &[], &[], &[]), table(Some("other"), "t", &[], &[], &[]), table(Some("public"), "u", &[], &[], &[])];
        let c = Catalog::new(&ts, "postgres");
        assert_eq!(c.table(&["other".into(), "t".into()]).unwrap().schema.as_deref(), Some("other"));
        assert_eq!(c.table(&["T".into()]).unwrap().schema.as_deref(), Some("public"));
        assert!(Catalog::new(&ts, "oracle").table(&["t".into()]).is_none());
        assert!(c.table(&["x".into()]).is_none());
    }

    #[test]
    fn keys_need_not_null_unique_columns() {
        let t = table(None, "t", &[("id", "int", false), ("a", "int", true), ("b", "int", false)], &["id"], &[(&["a"], true), (&["b"], true), (&["b"], false)]);
        assert_eq!(Catalog::keys(&t), vec![vec!["id".to_string()], vec!["b".to_string()]]);
        assert!(Catalog::leads_an_index(&t, "B"));
        assert!(!Catalog::leads_an_index(&t, "x"));
    }

    #[test]
    fn type_classes() {
        assert_eq!(type_class("nvarchar(50)"), TypeClass::Text);
        assert_eq!(type_class("character varying(20)"), TypeClass::Text);
        assert_eq!(type_class("DECIMAL(10,2)"), TypeClass::Number);
        assert_eq!(type_class("timestamp without time zone"), TypeClass::DateTime);
        assert_eq!(type_class("time"), TypeClass::Other);
        assert_eq!(index_column("\"fecha\" DESC"), "fecha");
    }
}
