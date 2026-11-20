//! Db2 LUW's `VECTOR(n, FLOAT32 | INT8)` (Db2 12.1.2 and later) through
//! ODBC / CLI.
//!
//! - **Values:** Db2 hands vectors to applications in their string form, a
//!   JSON-like array; cells and transfer reads are that text in the
//!   canonical form ([`dbine_driver::vector`]).
//! - **Types:** a column whose type name starts with `VECTOR` (`VECTOR`,
//!   `VECTOR(FLOAT32)` as `DESCRIBE TABLE` spells it, `VECTOR(3, INT8)`)
//!   is a vector; the catalog's length is its dimension. IBM's CLI (12.1.4)
//!   never says VECTOR, though: results, SQLColumns and the transfer's
//!   describe get VARCHAR (CLOB when the text passes 32,672 characters),
//!   so on Db2 12.1.2+ the vector columns come from `SYSCAT.COLUMNS`
//!   ([`result_types`], [`table_types`], [`CATALOG_SQL`]): by each result
//!   column's base table and column, which the CLI does report.
//! - **Literals and loads:** Db2 builds a vector from a string with
//!   `VECTOR(string, dimension, coordinate type)` (no implicit cast is
//!   documented): edits write `VECTOR('[…]', n, FLOAT32)` with the array's
//!   length, and loads bind each vector column as `VECTOR(?, n, type)` from
//!   the target column's type.
//!
//! An untyped edit can't tell a FLOAT32 column from an INT8 one, so it
//! writes FLOAT32, the common one (Db2 refuses it for an INT8 column);
//! scripts that know the columns' types ([`typed_literal`]) and transfers
//! (which read the target's types) write either.

use crate::ffi::{
    SQL_CLOB, SQL_DESC_BASE_COLUMN_NAME, SQL_DESC_BASE_TABLE_NAME, SQL_DESC_SCHEMA_NAME,
    SQL_LONGVARCHAR, SQL_VARCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR,
};
use crate::odbc::{Conn, Stmt, StmtSlot};
use dbine_driver::vector::{self, VectorElement, VectorMeta};
use dbine_driver::TableSchema;
use std::collections::HashMap;

/// Whether a type name is a vector's.
pub fn is_vector(type_name: &str) -> bool {
    type_name
        .trim()
        .get(..6)
        .is_some_and(|t| t.eq_ignore_ascii_case("VECTOR"))
}

/// A vector type's dimension and coordinate type, from its name and (when
/// the name doesn't say) the catalog's size. `None` when it isn't one.
fn shape(type_name: &str, size: Option<u64>) -> Option<(Option<u64>, &'static str)> {
    if !is_vector(type_name) {
        return None;
    }
    let inner = type_name.trim()[6..]
        .trim()
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or("");
    let mut dims = None;
    let mut coord = "FLOAT32";
    for part in inner.split(',').map(|p| p.trim().to_ascii_uppercase()) {
        match part.as_str() {
            "" => {}
            "INT8" => coord = "INT8",
            "FLOAT32" | "REAL" => coord = "FLOAT32",
            n => dims = n.parse::<u64>().ok().or(dims),
        }
    }
    Some((dims.or(size.filter(|&n| n > 0)), coord))
}

/// The declared type, `VECTOR(n, FLOAT32)`; `None` when it isn't a vector.
/// Without the dimension, the name as it came.
pub fn declared(type_name: &str, size: Option<u64>) -> Option<String> {
    let (dims, coord) = shape(type_name, size)?;
    Some(match dims {
        Some(n) => format!("VECTOR({n}, {coord})"),
        None => type_name.trim().to_string(),
    })
}

/// What a result column of this type says; `None` when it isn't a vector.
pub fn meta(type_name: &str) -> Option<VectorMeta> {
    let (dims, coord) = shape(type_name, None)?;
    let element = if coord == "INT8" {
        VectorElement::I8
    } else {
        VectorElement::F32
    };
    Some(VectorMeta::dense(
        dims.and_then(|n| u32::try_from(n).ok()),
        element,
    ))
}

/// Db2's text of a vector in the canonical form; as it came when it
/// doesn't read as one.
pub fn canonical(text: String) -> String {
    match vector::parse(&text) {
        // FLOAT32 or INT8 coordinates: the shortest float32 of each.
        Some(v) => vector::text(v.into_iter().map(|x| x as f32)),
        None => text,
    }
}

/// A vector edit's value as Db2's literal, `VECTOR('[1,2]', 2, FLOAT32)`;
/// `None` when `v` isn't a vector.
pub fn literal(v: &serde_json::Value) -> Option<String> {
    let values = vector::parse_value(v)?;
    if values.is_empty() {
        return None;
    }
    Some(format!(
        "VECTOR('{}', {}, FLOAT32)",
        vector::text(values.iter().copied()),
        values.len()
    ))
}

/// A vector value for a column of a known vector type (`type_name` as a
/// result or a describe gives it, `meta` its vector meta), from its numbers
/// or the grid's text: `VECTOR('[…]', n, FLOAT32 | INT8)` with the
/// column's coordinate type and dimension (the value's length when the
/// type doesn't say). `None` when the value isn't a vector.
pub fn typed_literal(
    type_name: &str,
    meta: Option<&VectorMeta>,
    v: &serde_json::Value,
) -> Option<String> {
    let values = vector::parse_value(v)?;
    if values.is_empty() {
        return None;
    }
    let up = type_name.to_ascii_uppercase();
    let coord = if up.contains("INT8") {
        "INT8"
    } else if up.contains("FLOAT32") || up.contains("REAL") {
        "FLOAT32"
    } else if meta.is_some_and(|m| m.element == VectorElement::I8) {
        "INT8"
    } else {
        "FLOAT32"
    };
    let dims = shape(type_name, None)
        .and_then(|(d, _)| d)
        .or_else(|| meta.and_then(|m| m.dims).map(u64::from))
        .unwrap_or(values.len() as u64);
    Some(format!(
        "VECTOR('{}', {dims}, {coord})",
        vector::text(values.iter().copied())
    ))
}

/// The VALUES item that loads a vector column of this type from a text
/// parameter, `VECTOR(?, n, FLOAT32)`; `None` when it isn't a vector or
/// its dimension isn't known (the parameter then goes as it is).
pub fn param(type_name: &str, size: Option<u64>) -> Option<String> {
    let (dims, coord) = shape(type_name, size)?;
    Some(format!("VECTOR(?, {}, {coord})", dims?))
}

// --------------------------------------------------------- the catalog

/// Every vector column of the database: schema, table, column, dimension
/// and coordinate type (Db2 12.1.2+'s `SYSCAT.COLUMNS`).
pub const CATALOG_SQL: &str = "SELECT TRIM(TABSCHEMA), TABNAME, COLNAME, LENGTH, COORDINATETYPE \
     FROM SYSCAT.COLUMNS WHERE TYPENAME = 'VECTOR'";

/// One table's vector columns: column, dimension and coordinate type.
const TABLE_SQL: &str = "SELECT COLNAME, LENGTH, COORDINATETYPE FROM SYSCAT.COLUMNS \
     WHERE TYPENAME = 'VECTOR' AND TABSCHEMA = ? AND TABNAME = ?";

/// Whether a Db2 LUW server of this `SQL_DBMS_VER` (`12.01.0500`) has
/// vectors: 12.1.2 (`12.01.0200`) and later.
pub fn server_has_vectors(dbms_ver: &str) -> bool {
    let mut parts = dbms_ver.trim().split('.').map(|p| p.trim().parse::<u32>());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(level))) => {
            (major, minor, level) >= (12, 1, 200)
        }
        _ => false,
    }
}

/// The declared type of a catalog row's vector, `VECTOR(n, FLOAT32 | INT8)`
/// (`length` is its dimension).
pub fn catalog_type(length: &str, coordinate: &str) -> Option<String> {
    let n = length.trim().parse::<u64>().ok().filter(|&n| n > 0)?;
    declared(&format!("VECTOR({})", coordinate.trim()), Some(n))
}

/// Whether a result column the CLI describes as text could be a vector:
/// the CLI sizes a vector's text at 16 characters a FLOAT32 coordinate and
/// 5 an INT8 one, plus one (`VECTOR(3, FLOAT32)` comes as VARCHAR(49),
/// `VECTOR(4, INT8)` as VARCHAR(21), `VECTOR(3072, FLOAT32)` as a CLOB of
/// 49,153). Only those are looked up in the catalog.
pub fn may_be_vector(sql_type: i16, size: u64) -> bool {
    matches!(
        sql_type,
        SQL_VARCHAR | SQL_WVARCHAR | SQL_LONGVARCHAR | SQL_WLONGVARCHAR | SQL_CLOB
    ) && size > 1
        && ((size - 1).is_multiple_of(16) || (size - 1).is_multiple_of(5))
}

/// The vector columns of a table, by name, with their declared types;
/// empty when the catalog can't be read. Runs on a statement of its own,
/// so a result being read stays open (and cancellable through its slot).
pub fn table_types(c: &Conn, schema: &str, table: &str) -> HashMap<String, String> {
    let side = StmtSlot::default();
    let rows = c.stmt(&side).and_then(|st| {
        st.exec_params(TABLE_SQL, &[schema, table])?;
        st.text_rows()
    });
    match rows {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| {
                let get = |i: usize| r.get(i).cloned().flatten();
                Some((
                    get(0)?,
                    catalog_type(&get(1)?, &get(2).unwrap_or_default())?,
                ))
            })
            .collect(),
        Err(e) => {
            tracing::debug!("odbc: Db2 vector columns of {schema}.{table} not read: {e}");
            HashMap::new()
        }
    }
}

/// The declared vector type of each of `st`'s result columns (`cols`: each
/// one's SQL type and size, in order), from the catalog by its base table
/// and column; `None` for the others, and for all of them unless the
/// connection looks vectors up ([`Conn::vector_catalog`]). A vector built
/// in the query (no base column) stays text.
pub fn result_types(c: &Conn, st: &Stmt, cols: &[(i16, u64)]) -> Vec<Option<String>> {
    let mut out = vec![None; cols.len()];
    if !c.vector_catalog {
        return out;
    }
    let mut tables: HashMap<(String, String), HashMap<String, String>> = HashMap::new();
    for (i, &(sql_type, size)) in cols.iter().enumerate() {
        if !may_be_vector(sql_type, size) {
            continue;
        }
        let n = i as u16 + 1;
        let attr = |field| st.col_text(n, field).filter(|t| !t.trim().is_empty());
        let (Some(schema), Some(table), Some(column)) = (
            attr(SQL_DESC_SCHEMA_NAME),
            attr(SQL_DESC_BASE_TABLE_NAME),
            attr(SQL_DESC_BASE_COLUMN_NAME),
        ) else {
            continue;
        };
        let schema = schema.trim().to_string();
        let types = tables
            .entry((schema.clone(), table.clone()))
            .or_insert_with(|| table_types(c, &schema, &table));
        out[i] = types.get(&column).cloned();
    }
    out
}

/// [`CATALOG_SQL`]'s rows, empty unless the connection looks vectors up
/// or when the catalog can't be read.
pub fn catalog_rows(c: &Conn) -> Vec<Vec<Option<String>>> {
    if !c.vector_catalog {
        return Vec::new();
    }
    let side = StmtSlot::default();
    c.stmt(&side)
        .and_then(|st| {
            st.exec(CATALOG_SQL)?;
            st.text_rows()
        })
        .unwrap_or_else(|e| {
            tracing::debug!("odbc: Db2 vector columns not read: {e}");
            Vec::new()
        })
}

/// Puts [`CATALOG_SQL`]'s types on the columns of a schema read through
/// SQLColumns (where they are VARCHAR or CLOB).
pub fn attach(tables: &mut [TableSchema], rows: &[Vec<Option<String>>]) {
    for r in rows {
        let get = |i: usize| r.get(i).cloned().flatten();
        let (Some(schema), Some(table), Some(column), Some(length)) =
            (get(0), get(1), get(2), get(3))
        else {
            continue;
        };
        let Some(ty) = catalog_type(&length, &get(4).unwrap_or_default()) else {
            continue;
        };
        let columns = tables
            .iter_mut()
            .filter(|t| t.name == table && t.schema.as_deref().is_none_or(|s| s == schema))
            .flat_map(|t| t.columns.iter_mut())
            .filter(|c| c.name == column);
        for c in columns {
            c.data_type = ty.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;
    use serde_json::json;

    #[test]
    fn types_as_db2_spells_them() {
        assert!(is_vector("VECTOR"));
        assert!(is_vector("vector(3, int8)"));
        assert!(!is_vector("VARCHAR"));
        assert!(!is_vector("VECT"));
        assert_eq!(
            declared("VECTOR", Some(1536)).as_deref(),
            Some("VECTOR(1536, FLOAT32)")
        );
        assert_eq!(
            declared("VECTOR(INT8)", Some(300)).as_deref(),
            Some("VECTOR(300, INT8)")
        );
        assert_eq!(
            declared("VECTOR(REAL)", Some(3)).as_deref(),
            Some("VECTOR(3, FLOAT32)")
        );
        assert_eq!(
            declared("VECTOR(3, INT8)", None).as_deref(),
            Some("VECTOR(3, INT8)")
        );
        assert_eq!(declared("VECTOR", None).as_deref(), Some("VECTOR"));
        assert_eq!(declared("INTEGER", Some(4)), None);
        assert_eq!(
            meta("VECTOR(3, FLOAT32)"),
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(
            meta("VECTOR(INT8)"),
            Some(VectorMeta::dense(None, VectorElement::I8))
        );
        assert_eq!(meta("CLOB"), None);
    }

    #[test]
    fn values_literals_and_loads() {
        assert_eq!(canonical("[1.0E0, 2.5E0, -3]".into()), "[1,2.5,-3]");
        assert_eq!(canonical("[1.0000000149011612E-1]".into()), "[0.1]");
        assert_eq!(canonical("no".into()), "no");
        assert_eq!(
            literal(&json!([0.5, 2.0, -1.0])).as_deref(),
            Some("VECTOR('[0.5,2,-1]', 3, FLOAT32)")
        );
        assert_eq!(literal(&json!([])), None);
        assert_eq!(literal(&json!("x")), None);
        assert_eq!(
            param("VECTOR", Some(3)).as_deref(),
            Some("VECTOR(?, 3, FLOAT32)")
        );
        assert_eq!(
            param("VECTOR(INT8)", Some(4)).as_deref(),
            Some("VECTOR(?, 4, INT8)")
        );
        assert_eq!(param("VECTOR", None), None);
        assert_eq!(param("INTEGER", Some(4)), None);
    }

    #[test]
    fn typed_literals_take_the_columns_coordinate_type() {
        // From a describe's type; from a result's meta; from the grid's text.
        assert_eq!(
            typed_literal("VECTOR(3, INT8)", None, &json!([1, -2, 3])).as_deref(),
            Some("VECTOR('[1,-2,3]', 3, INT8)")
        );
        let int8 = VectorMeta::dense(Some(2), VectorElement::I8);
        assert_eq!(
            typed_literal("VECTOR", Some(&int8), &json!("[4,5]")).as_deref(),
            Some("VECTOR('[4,5]', 2, INT8)")
        );
        assert_eq!(
            typed_literal("VECTOR(FLOAT32)", None, &json!([0.5, 1])).as_deref(),
            Some("VECTOR('[0.5,1]', 2, FLOAT32)")
        );
        assert_eq!(typed_literal("VECTOR(3, INT8)", None, &json!("x")), None);
        assert_eq!(typed_literal("VECTOR(3, INT8)", None, &json!([])), None);
    }

    #[test]
    fn db2_servers_with_vectors() {
        assert!(server_has_vectors("12.01.0200"));
        assert!(server_has_vectors("12.01.0500"));
        assert!(server_has_vectors("13.01.0000"));
        assert!(!server_has_vectors("12.01.0100"));
        assert!(!server_has_vectors("11.05.0900"));
        assert!(!server_has_vectors(""));
    }

    #[test]
    fn catalog_rows_declare_the_type() {
        assert_eq!(
            catalog_type("3", "FLOAT32").as_deref(),
            Some("VECTOR(3, FLOAT32)")
        );
        assert_eq!(
            catalog_type(" 4 ", "INT8 ").as_deref(),
            Some("VECTOR(4, INT8)")
        );
        assert_eq!(
            catalog_type("1536", "").as_deref(),
            Some("VECTOR(1536, FLOAT32)")
        );
        assert_eq!(catalog_type("0", "INT8"), None);
        assert_eq!(catalog_type("x", "INT8"), None);
    }

    /// The sizes IBM's CLI 12.1.4 gave Db2 12.1.5's vectors (VARCHAR, and
    /// CLOB past 32,672 characters): VECTOR(1 | 3 | 1536 | 2042 | 3072,
    /// FLOAT32) and VECTOR(1 | 4 | 300 | 6535 | 8000, INT8).
    #[test]
    fn the_clis_text_sizes_of_vectors_are_looked_up() {
        for (sql_type, size) in [
            (SQL_VARCHAR, 17),
            (SQL_VARCHAR, 49),
            (SQL_VARCHAR, 24577),
            (SQL_CLOB, 32673),
            (SQL_CLOB, 49153),
            (SQL_VARCHAR, 6),
            (SQL_WVARCHAR, 21),
            (SQL_VARCHAR, 1501),
            (SQL_CLOB, 32676),
            (SQL_CLOB, 40001),
        ] {
            assert!(may_be_vector(sql_type, size), "{sql_type} {size}");
        }
        // Common text sizes and other types aren't.
        for size in [10, 50, 100, 255, 1000, 4000, 32672] {
            assert!(!may_be_vector(SQL_VARCHAR, size), "{size}");
        }
        assert!(!may_be_vector(crate::ffi::SQL_INTEGER, 21));
        assert!(!may_be_vector(SQL_VARCHAR, 1));
    }

    #[test]
    fn the_catalog_types_a_schema_read_through_sqlcolumns() {
        let col = |name: &str, ty: &str| ColumnDef {
            name: name.into(),
            data_type: ty.into(),
            ..Default::default()
        };
        let table = |schema: &str, name: &str| TableSchema {
            schema: Some(schema.into()),
            name: name.into(),
            columns: vec![
                col("ID", "INTEGER"),
                col("F", "VARCHAR(49)"),
                col("I", "VARCHAR(21)"),
            ],
            ..Default::default()
        };
        let mut tables = vec![table("APP", "T"), table("OTHER", "T")];
        let row = |v: [&str; 5]| v.iter().map(|s| Some(s.to_string())).collect::<Vec<_>>();
        attach(
            &mut tables,
            &[
                row(["APP", "T", "F", "3", "FLOAT32"]),
                row(["APP", "T", "I", "4", "INT8"]),
                row(["APP", "GONE", "F", "3", "FLOAT32"]),
            ],
        );
        let types = |t: &TableSchema| {
            t.columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            types(&tables[0]),
            ["INTEGER", "VECTOR(3, FLOAT32)", "VECTOR(4, INT8)"]
        );
        assert_eq!(types(&tables[1]), ["INTEGER", "VARCHAR(49)", "VARCHAR(21)"]);
    }
}
